-------------------------- MODULE PagerVmaPublication --------------------------
EXTENDS Naturals

(*******************************************************************************
Owner: kernel-ps pager VMA publication.
Writers are serialized; exception readers observe all-atomic sequence snapshots
and admit only an exact live process/MM/VMA identity with allowed access.
*******************************************************************************)

CONSTANTS Empty, Writing, Published, Revoked

VARIABLES state, sequence, vmaGeneration, overlap, writable, executable,
          lookupAdmitted, admittedIdentityExact, admittedPermissionAllowed
vars == <<state, sequence, vmaGeneration, overlap, writable, executable,
          lookupAdmitted, admittedIdentityExact, admittedPermissionAllowed>>

Init ==
    /\ state = Empty
    /\ sequence = 0
    /\ vmaGeneration = 0
    /\ overlap = FALSE
    /\ writable = FALSE
    /\ executable = FALSE
    /\ lookupAdmitted = FALSE
    /\ admittedIdentityExact = TRUE
    /\ admittedPermissionAllowed = TRUE

BeginPublish(mayWrite, mayExecute, hasOverlap) ==
    /\ state \in {Empty, Revoked}
    /\ sequence < 4
    /\ sequence % 2 = 0
    /\ ~(mayWrite /\ mayExecute)
    /\ ~hasOverlap
    /\ state' = Writing
    /\ sequence' = sequence + 1
    /\ vmaGeneration' = vmaGeneration + 1
    /\ overlap' = hasOverlap
    /\ writable' = mayWrite
    /\ executable' = mayExecute
    /\ lookupAdmitted' = FALSE
    /\ admittedIdentityExact' = TRUE
    /\ admittedPermissionAllowed' = TRUE

Commit ==
    /\ state = Writing
    /\ sequence % 2 = 1
    /\ state' = Published
    /\ sequence' = sequence + 1
    /\ UNCHANGED <<vmaGeneration, overlap, writable, executable,
                    lookupAdmitted, admittedIdentityExact,
                    admittedPermissionAllowed>>

Lookup(identityExact, permissionAllowed) ==
    /\ state = Published
    /\ sequence % 2 = 0
    /\ identityExact
    /\ permissionAllowed
    /\ lookupAdmitted' = TRUE
    /\ admittedIdentityExact' = identityExact
    /\ admittedPermissionAllowed' = permissionAllowed
    /\ UNCHANGED <<state, sequence, vmaGeneration, overlap, writable,
                    executable>>

RejectLookup(identityExact, permissionAllowed) ==
    /\ state \in {Writing, Published, Revoked}
    /\ \/ state # Published
       \/ sequence % 2 = 1
       \/ ~identityExact
       \/ ~permissionAllowed
    /\ UNCHANGED vars

Revoke ==
    /\ state = Published
    /\ sequence < 5
    /\ sequence % 2 = 0
    /\ state' = Revoked
    /\ sequence' = sequence + 2
    /\ lookupAdmitted' = FALSE
    /\ UNCHANGED <<vmaGeneration, overlap, writable, executable,
                    admittedIdentityExact, admittedPermissionAllowed>>

ObserveTerminal ==
    /\ \/ state = Published
       \/ /\ state = Revoked
          /\ sequence >= 4
    /\ UNCHANGED vars

Next ==
    \/ \E mayWrite \in BOOLEAN, mayExecute \in BOOLEAN, hasOverlap \in BOOLEAN:
         BeginPublish(mayWrite, mayExecute, hasOverlap)
    \/ Commit
    \/ \E identityExact \in BOOLEAN, permissionAllowed \in BOOLEAN:
         Lookup(identityExact, permissionAllowed)
    \/ \E identityExact \in BOOLEAN, permissionAllowed \in BOOLEAN:
         RejectLookup(identityExact, permissionAllowed)
    \/ Revoke
    \/ ObserveTerminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {Empty, Writing, Published, Revoked}
    /\ sequence \in 0..6
    /\ vmaGeneration \in 0..2
    /\ overlap \in BOOLEAN
    /\ writable \in BOOLEAN
    /\ executable \in BOOLEAN
    /\ lookupAdmitted \in BOOLEAN
    /\ admittedIdentityExact \in BOOLEAN
    /\ admittedPermissionAllowed \in BOOLEAN

PublishedSequenceIsStable == (state = Published) => sequence % 2 = 0
PublishedVmaIsNonoverlapping == (state = Published) => ~overlap
PublishedRightsAreWxSafe == (state = Published) => ~(writable /\ executable)
PublishedVmaGenerationIsNonzero == (state = Published) => vmaGeneration > 0
LookupRequiresExactIdentity == lookupAdmitted => admittedIdentityExact
LookupRequiresAllowedAccess == lookupAdmitted => admittedPermissionAllowed
RevokedVmaIsNotAdmitted == (state = Revoked) => ~lookupAdmitted

=============================================================================
