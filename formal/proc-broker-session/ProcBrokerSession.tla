--------------------------- MODULE ProcBrokerSession ---------------------------
EXTENDS Naturals

(*******************************************************************************
Models the loader-owned process-broker session.

Concrete owners:
  * kernel/compat/src/user/syscall/linux/proc_broker_ops.rs
  * services/loaderd/src/main.rs
  * kernel/compat/src/user/syscall/linux.rs
  * kernel/compat/src/user/syscall/linux/support.rs

PREPARE allocates an exact owner-bound handle. MAP and runtime-metadata calls
may mutate only that live session. COMMIT consumes it before spawning a normal
or deferred child; a rejected commit is terminal too, matching the concrete
remove-before-validation linearization. ABORT and process exit discard the
uncommitted mapping state, including pinned backing metadata, so a crashed
loader cannot exhaust the bounded prepare table.

This model covers session authority/lifecycle rather than ELF/PE bytes,
page-table contents, or loaderd's format parsers.
*******************************************************************************)

CONSTANTS Owners, Sessions, Pids, MaxMappings, ConsoleSessions

NoOwner == 0
NoPid == 0
NoConsole == 0

\* The checked finite instance has two owner contexts and two distinct console
\* sessions. The concrete invariant is equality with the caller's context, not
\* the particular numeric handles used here.
OwnerConsole(owner) == owner + 9

Alive == "alive"
Exited == "exited"

Free == "free"
Prepared == "prepared"
Mapped == "mapped"
Ready == "ready"
Committed == "committed"
Aborted == "aborted"
CommitRejected == "commit-rejected"

Absent == "absent"
Suspended == "suspended"
Running == "running"
ChildExited == "child-exited"

VARIABLES ownerState,
          sessionState,
          sessionOwner,
          mappingCount,
          runtimeSet,
          childState,
          childSession,
          childDeferred,
          childConsole,
          childInheritedConsole

vars == <<ownerState, sessionState, sessionOwner, mappingCount, runtimeSet,
          childState, childSession, childDeferred, childConsole,
          childInheritedConsole>>

Uncommitted(state) == state \in {Prepared, Mapped, Ready}

Init ==
    /\ ownerState = [owner \in Owners |-> Alive]
    /\ sessionState = [session \in Sessions |-> Free]
    /\ sessionOwner = [session \in Sessions |-> NoOwner]
    /\ mappingCount = [session \in Sessions |-> 0]
    /\ runtimeSet = [session \in Sessions |-> FALSE]
    /\ childState = [pid \in Pids |-> Absent]
    /\ childSession = [pid \in Pids |-> NoOwner]
    /\ childDeferred = [pid \in Pids |-> FALSE]
    /\ childConsole = [pid \in Pids |-> NoConsole]
    /\ childInheritedConsole = [pid \in Pids |-> FALSE]

Prepare(owner, session) ==
    /\ owner \in Owners
    /\ session \in Sessions
    /\ ownerState[owner] = Alive
    /\ sessionState[session] = Free
    /\ sessionState' = [sessionState EXCEPT ![session] = Prepared]
    /\ sessionOwner' = [sessionOwner EXCEPT ![session] = owner]
    /\ UNCHANGED <<ownerState, mappingCount, runtimeSet, childState, childSession,
                  childDeferred, childConsole, childInheritedConsole>>

MapSegment(owner, session) ==
    /\ owner \in Owners
    /\ session \in Sessions
    /\ ownerState[owner] = Alive
    /\ sessionOwner[session] = owner
    /\ sessionState[session] \in {Prepared, Mapped}
    /\ mappingCount[session] < MaxMappings
    /\ sessionState' = [sessionState EXCEPT ![session] = Mapped]
    /\ mappingCount' = [mappingCount EXCEPT ![session] = mappingCount[session] + 1]
    /\ UNCHANGED <<ownerState, sessionOwner, runtimeSet, childState, childSession,
                  childDeferred, childConsole, childInheritedConsole>>

SetRuntimeMetadata(owner, session) ==
    /\ owner \in Owners
    /\ session \in Sessions
    /\ ownerState[owner] = Alive
    /\ sessionOwner[session] = owner
    /\ sessionState[session] = Mapped
    /\ mappingCount[session] > 0
    /\ runtimeSet[session] = FALSE
    /\ sessionState' = [sessionState EXCEPT ![session] = Ready]
    /\ runtimeSet' = [runtimeSet EXCEPT ![session] = TRUE]
    /\ UNCHANGED <<ownerState, sessionOwner, mappingCount, childState,
                  childSession, childDeferred, childConsole, childInheritedConsole>>

Abort(owner, session) ==
    /\ owner \in Owners
    /\ session \in Sessions
    /\ ownerState[owner] = Alive
    /\ sessionOwner[session] = owner
    /\ Uncommitted(sessionState[session])
    /\ sessionState' = [sessionState EXCEPT ![session] = Aborted]
    /\ mappingCount' = [mappingCount EXCEPT ![session] = 0]
    /\ runtimeSet' = [runtimeSet EXCEPT ![session] = FALSE]
    /\ UNCHANGED <<ownerState, sessionOwner, childState, childSession,
                  childDeferred, childConsole, childInheritedConsole>>

(*******************************************************************************
Commit removes the prepare handle before the subsequent argument/image checks.
Therefore a rejected commit cannot be retried or aborted through the old
handle; it releases the same mapping state as a successful commit.
*******************************************************************************)
RejectCommit(owner, session) ==
    /\ owner \in Owners
    /\ session \in Sessions
    /\ ownerState[owner] = Alive
    /\ sessionOwner[session] = owner
    /\ sessionState[session] = Ready
    /\ sessionState' = [sessionState EXCEPT ![session] = CommitRejected]
    /\ mappingCount' = [mappingCount EXCEPT ![session] = 0]
    /\ runtimeSet' = [runtimeSet EXCEPT ![session] = FALSE]
    /\ UNCHANGED <<ownerState, sessionOwner, childState, childSession,
                  childDeferred, childConsole, childInheritedConsole>>

Commit(owner, session, pid, deferred, requestedConsole) ==
    /\ owner \in Owners
    /\ session \in Sessions
    /\ pid \in Pids
    /\ deferred \in BOOLEAN
    /\ requestedConsole \in ConsoleSessions \cup {NoConsole}
    /\ ownerState[owner] = Alive
    /\ sessionOwner[session] = owner
    /\ sessionState[session] = Ready
    /\ childState[pid] = Absent
    /\ sessionState' = [sessionState EXCEPT ![session] = Committed]
    /\ mappingCount' = [mappingCount EXCEPT ![session] = 0]
    /\ runtimeSet' = [runtimeSet EXCEPT ![session] = FALSE]
    /\ childState' = [childState EXCEPT ![pid] = IF deferred THEN Suspended ELSE Running]
    /\ childSession' = [childSession EXCEPT ![pid] = session]
    /\ childDeferred' = [childDeferred EXCEPT ![pid] = deferred]
    /\ childConsole' = [childConsole EXCEPT ![pid] =
        IF requestedConsole = NoConsole THEN OwnerConsole(owner) ELSE requestedConsole]
    /\ childInheritedConsole' = [childInheritedConsole EXCEPT ![pid] =
        requestedConsole = NoConsole]
    /\ UNCHANGED <<ownerState, sessionOwner>>

ActivateDeferredChild(pid) ==
    /\ pid \in Pids
    /\ childState[pid] = Suspended
    /\ childDeferred[pid]
    /\ childState' = [childState EXCEPT ![pid] = Running]
    /\ UNCHANGED <<ownerState, sessionState, sessionOwner, mappingCount,
                  runtimeSet, childSession, childDeferred, childConsole,
                  childInheritedConsole>>

ExitChild(pid) ==
    /\ pid \in Pids
    /\ childState[pid] \in {Suspended, Running}
    /\ childState' = [childState EXCEPT ![pid] = ChildExited]
    /\ UNCHANGED <<ownerState, sessionState, sessionOwner, mappingCount,
                  runtimeSet, childSession, childDeferred, childConsole,
                  childInheritedConsole>>

(*******************************************************************************
Rust process teardown removes all live ProcPrepareState entries for the exiting
owner. Committed children already own their prepared address space, so only the
uncommitted sessions are aborted here.
*******************************************************************************)
ExitOwner(owner) ==
    /\ owner \in Owners
    /\ ownerState[owner] = Alive
    /\ ownerState' = [ownerState EXCEPT ![owner] = Exited]
    /\ sessionState' =
        [session \in Sessions |->
            IF sessionOwner[session] = owner /\ Uncommitted(sessionState[session])
            THEN Aborted ELSE sessionState[session]]
    /\ mappingCount' =
        [session \in Sessions |->
            IF sessionOwner[session] = owner /\ Uncommitted(sessionState[session])
            THEN 0 ELSE mappingCount[session]]
    /\ runtimeSet' =
        [session \in Sessions |->
            IF sessionOwner[session] = owner /\ Uncommitted(sessionState[session])
            THEN FALSE ELSE runtimeSet[session]]
    /\ UNCHANGED <<sessionOwner, childState, childSession, childDeferred,
                  childConsole, childInheritedConsole>>

Next ==
    \/ \E owner \in Owners, session \in Sessions : Prepare(owner, session)
    \/ \E owner \in Owners, session \in Sessions : MapSegment(owner, session)
    \/ \E owner \in Owners, session \in Sessions : SetRuntimeMetadata(owner, session)
    \/ \E owner \in Owners, session \in Sessions : Abort(owner, session)
    \/ \E owner \in Owners, session \in Sessions : RejectCommit(owner, session)
    \/ \E owner \in Owners, session \in Sessions, pid \in Pids, deferred \in BOOLEAN,
           requestedConsole \in ConsoleSessions \cup {NoConsole} :
        Commit(owner, session, pid, deferred, requestedConsole)
    \/ \E pid \in Pids : ActivateDeferredChild(pid)
    \/ \E pid \in Pids : ExitChild(pid)
    \/ \E owner \in Owners : ExitOwner(owner)

TypeOK ==
    /\ Owners \subseteq Nat
    /\ NoOwner \notin Owners
    /\ Sessions \subseteq Nat
    /\ Pids \subseteq Nat
    /\ NoPid \notin Pids
    /\ MaxMappings \in Nat
    /\ ConsoleSessions \subseteq Nat
    /\ NoConsole \notin ConsoleSessions
    /\ \A owner \in Owners : OwnerConsole(owner) \in ConsoleSessions
    /\ ownerState \in [Owners -> {Alive, Exited}]
    /\ sessionState \in [Sessions -> {Free, Prepared, Mapped, Ready, Committed,
                                      Aborted, CommitRejected}]
    /\ sessionOwner \in [Sessions -> Owners \cup {NoOwner}]
    /\ mappingCount \in [Sessions -> 0..MaxMappings]
    /\ runtimeSet \in [Sessions -> BOOLEAN]
    /\ childState \in [Pids -> {Absent, Suspended, Running, ChildExited}]
    /\ childSession \in [Pids -> Sessions \cup {NoOwner}]
    /\ childDeferred \in [Pids -> BOOLEAN]
    /\ childConsole \in [Pids -> ConsoleSessions \cup {NoConsole}]
    /\ childInheritedConsole \in [Pids -> BOOLEAN]

EveryLiveSessionHasItsExactAliveOwner ==
    \A session \in Sessions :
        Uncommitted(sessionState[session]) =>
            /\ sessionOwner[session] # NoOwner
            /\ ownerState[sessionOwner[session]] = Alive

OnlyTheExactOwnerCanMutateAReadySession ==
    \A session \in Sessions :
        sessionState[session] = Ready =>
            /\ mappingCount[session] > 0
            /\ runtimeSet[session]
            /\ sessionOwner[session] # NoOwner

TerminalSessionReleasesPinnedPreparationState ==
    \A session \in Sessions :
        sessionState[session] \in {Committed, Aborted, CommitRejected} =>
            /\ mappingCount[session] = 0
            /\ ~runtimeSet[session]

CommittedSessionCreatesExactlyOneChild ==
    \A pid \in Pids :
        childState[pid] # Absent =>
            /\ childSession[pid] \in Sessions
            /\ sessionState[childSession[pid]] = Committed

DeferredChildIsInertUntilExplicitActivation ==
    \A pid \in Pids :
        childState[pid] = Suspended =>
            /\ childDeferred[pid]
            /\ sessionState[childSession[pid]] = Committed

\* A zero console-session argument means inherit the caller's exact current
\* session. There is deliberately no rule that replaces a missing caller
\* context with the privileged system session.
InheritedConsoleIsExactOwnerSession ==
    \A pid \in Pids :
        childState[pid] # Absent /\ childInheritedConsole[pid] =>
            /\ childSession[pid] \in Sessions
            /\ sessionOwner[childSession[pid]] \in Owners
            /\ childConsole[pid] = OwnerConsole(sessionOwner[childSession[pid]])

ExitedOwnerRetainsNoLivePrepareAuthority ==
    \A owner \in Owners :
        ownerState[owner] = Exited =>
            \A session \in Sessions :
                sessionOwner[session] = owner => ~Uncommitted(sessionState[session])

=============================================================================
