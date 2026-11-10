-------------------- MODULE SchedulerOwnerWord --------------------
EXTENDS Naturals

(***************************************************************************
Owner: the per-task scheduler owner word, `RunOwnerWord` in
kernel/ps/src/multitask/scheduler/runqueue.rs.
Linearization points: every successful compare-exchange of the word. A
lock-free writer is a load, a pure decision on the loaded word, and an exact
compare-exchange against it; a lost CAS changes nothing and retries on the new
word. The reachable effect of that loop is therefore the decision applied
atomically to the CURRENT word, which is how the metadata writers below are
modeled. The consumers whose snapshot is held across other steps are NOT
collapsed: retire completion, slot release, mailbox adoption and the lane's
claim are two-step actions (observe, then exact CAS) so a writer can land
between them.

Concrete owners:
  * runqueue/ownership.rs  transition (TRANSITIONS table), edit_in_place
                     (set_runnable, set_wait_armed, set_wait_reason,
                     clear_wait_arm, publish_wait_arm, commit_wait,
                     wake_wait_locked)
  * runqueue.rs      retire_slot, release_retired
  * runqueue/ipc_handoff.rs   claim_blocked_handoff_duty, publish_remote_wake,
                     drain_remote_wakes (mailbox adoption), publish_migrating_record
  * runqueue/lane.rs lane_claim
  * runqueue/claim.rs claim_dispatch_inner,
    runqueue/return_publication.rs publish_return

Assumption (outside this model, owned by terminal_quarantine): a slot has one
terminal retirer at a time, so two retire_slot calls never interleave.

The hazard this model exists for: a metadata write that lands on a Retiring or
Retired word makes the exact-value retire completion (or slot release) lose and
panic. `current_return.publish_frame` raises run intent after the owner may
already have been withdrawn by a remote stop, so that write is real. Every
panic site is a `crash` reason; a terminal word must always be the canonical
word its exact consumer will compare against.
***************************************************************************)

CONSTANT MaxGen

Cpus == 1..3
NoCpu == 0

Dormant == "dormant"
Local == "local"
RemoteQueued == "remote-queued"
Running == "running"
Migrating == "migrating"
Blocked == "blocked"
Retiring == "retiring"
Retired == "retired"
DirectHandoff == "direct-handoff"

States == {Dormant, Local, RemoteQueued, Running, Migrating, Blocked, Retiring, Retired, DirectHandoff}
Terminal == {Retiring, Retired}
\* States whose word carries run intent when first entered.
RunnableStates == {Local, RemoteQueued, Running, Migrating, DirectHandoff}

Crashes == {"none", "retire-completion-lost", "release-raced", "adoption-lost",
            "migration-lost", "mailbox-record-lost"}

VARIABLES w, prevGen, retPhase, retCpu, retGen, relPhase, relGen,
          adoptPhase, adoptRec, adoptObs, mbox, lanePhase, laneSnap,
          migPhase, migGen, migCpu, crash

vars == <<w, prevGen, retPhase, retCpu, retGen, relPhase, relGen,
          adoptPhase, adoptRec, adoptObs, mbox, lanePhase, laneSnap,
          migPhase, migGen, migCpu, crash>>

Word(st, c, run, arm, rsn, g) == [st |-> st, cpu |-> c, run |-> run, arm |-> arm, rsn |-> rsn, gen |-> g]

\* `next()`: a new generation with all metadata reset.
Fresh(st, c) == Word(st, c, st \in RunnableStates, FALSE, 0, w.gen + 1)
\* `next_preserving_wait()`: a new generation that keeps the wait arm and reason.
Preserve(st, c) == Word(st, c, st \in RunnableStates, w.arm, w.rsn, w.gen + 1)

RetiringWord(c, g) == Word(Retiring, c, FALSE, FALSE, 0, g)
RetiredWord(g) == Word(Retired, NoCpu, FALSE, FALSE, 0, g)

CanAdvance == w.gen < MaxGen
\* Two-step actions reserve their completion generation when they start, so the
\* finite generation bound never reads as a lost CAS.
CanAdvanceTwice == w.gen + 2 <= MaxGen
\* `RunOwnerState::is_editable`: an admitted task that has not been withdrawn.
\* A remote stop took the task off the generation a consumer was working on,
\* possibly already released and reused: the consumer rolls back and gives up.
Withdrawn(g) == w.st \in Terminal \/ w.gen > g
Editable == w.st \notin Terminal \cup {Dormant}

TypeOK ==
    /\ w.st \in States
    /\ w.cpu \in Cpus \cup {NoCpu}
    /\ w.run \in BOOLEAN
    /\ w.arm \in BOOLEAN
    /\ w.rsn \in 0..1
    /\ w.gen \in 1..MaxGen
    /\ prevGen \in 1..MaxGen
    /\ retPhase \in {"idle", "mid"}
    /\ relPhase \in {"idle", "mid"}
    /\ adoptPhase \in {"idle", "loaded"}
    /\ lanePhase \in {"idle", "observed"}
    /\ migPhase \in {"idle", "mid"}
    /\ mbox \subseteq 1..MaxGen
    /\ crash \in Crashes

Init ==
    /\ w = Word(Dormant, NoCpu, FALSE, FALSE, 0, 1)
    /\ prevGen = 1
    /\ retPhase = "idle" /\ retCpu = NoCpu /\ retGen = 1
    /\ relPhase = "idle" /\ relGen = 1
    /\ adoptPhase = "idle" /\ adoptRec = 1 /\ adoptObs = w
    /\ mbox = {}
    /\ lanePhase = "idle" /\ laneSnap = w
    /\ migPhase = "idle" /\ migGen = 1 /\ migCpu = NoCpu
    /\ crash = "none"

\* Every step records the generation it started from.
Step(action) == action /\ prevGen' = w.gen

Keep(x) == UNCHANGED x

(* ---- custody transitions: a strictly newer generation ---- *)

Admit ==
    /\ w.st = Dormant /\ CanAdvance
    /\ w' = Fresh(Blocked, NoCpu)
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

LocalWake(c) ==
    /\ w.st = Blocked /\ CanAdvance
    /\ w' = Fresh(Local, c)
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

RemoteWake(c) ==
    /\ w.st = Blocked /\ CanAdvance
    /\ w' = Preserve(RemoteQueued, c)
    /\ mbox' = mbox \cup {w.gen + 1}
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

DirectWake(c) ==
    /\ w.st = Blocked /\ ~w.arm /\ CanAdvance
    /\ w' = Preserve(DirectHandoff, c)
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

DirectToLocal ==
    /\ w.st = DirectHandoff /\ w.run /\ CanAdvance
    /\ w' = Fresh(Local, w.cpu)
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

DirectRollback ==
    /\ w.st = DirectHandoff /\ w.run /\ CanAdvance
    /\ w' = Preserve(Blocked, NoCpu)
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

\* claim_dispatch_inner: Local or DirectHandoff -> Running on the owner's CPU.
Claim ==
    /\ w.st \in {Local, DirectHandoff} /\ w.run /\ CanAdvance
    /\ w' = Fresh(Running, w.cpu)
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

\* publish_return: the executing task goes back to its queue or is published Blocked.
Block ==
    /\ w.st = Running /\ CanAdvance
    /\ w' = IF w.run THEN Fresh(Local, w.cpu) ELSE Preserve(Blocked, NoCpu)
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

(* ---- migration: Local -> Migrating -> RemoteQueued, mailbox record at the end ---- *)

MigrateStart(c) ==
    /\ w.st = Local /\ w.cpu # c /\ CanAdvanceTwice /\ migPhase = "idle"
    /\ w' = Fresh(Migrating, c)
    /\ migPhase' = "mid" /\ migGen' = w.gen + 1 /\ migCpu' = c
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, lanePhase, laneSnap, crash>>

MigratePublish ==
    /\ migPhase = "mid"
    /\ IF w.st = Migrating /\ w.gen = migGen
          THEN /\ w' = Fresh(RemoteQueued, migCpu)
               /\ mbox' = mbox \cup {w.gen + 1}
               /\ crash' = crash
          ELSE /\ w' = w /\ mbox' = mbox
               /\ crash' = IF Withdrawn(migGen) THEN crash ELSE "migration-lost"
    /\ migPhase' = "idle"
    /\ UNCHANGED <<migGen, migCpu, retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec,
                   adoptObs, lanePhase, laneSnap>>

(* ---- mailbox adoption: load the owner, then an exact CAS that metadata or a stop can beat ---- *)

AdoptLoad ==
    /\ adoptPhase = "idle" /\ CanAdvance
    /\ \E rec \in mbox :
        /\ mbox' = mbox \ {rec}
        /\ adoptRec' = rec
        /\ IF w.st = RemoteQueued /\ w.gen = rec
              THEN /\ adoptPhase' = "loaded" /\ adoptObs' = w /\ crash' = crash
              ELSE /\ adoptPhase' = "idle" /\ adoptObs' = adoptObs
                   /\ crash' = IF Withdrawn(rec) THEN crash ELSE "mailbox-record-lost"
    /\ UNCHANGED <<w, retPhase, retCpu, retGen, relPhase, relGen, lanePhase, laneSnap, migPhase,
                   migGen, migCpu>>

AdoptCas ==
    /\ adoptPhase = "loaded"
    /\ IF w = adoptObs
          THEN /\ w' = Fresh(Local, w.cpu)
               /\ adoptPhase' = "idle" /\ adoptObs' = adoptObs /\ crash' = crash
          ELSE IF Withdrawn(adoptRec)
          THEN \* a remote stop won: the queue insertion is rolled back, the task is gone
               /\ w' = w /\ adoptPhase' = "idle" /\ adoptObs' = adoptObs /\ crash' = crash
          ELSE IF w.st = RemoteQueued /\ w.cpu = adoptObs.cpu /\ w.gen = adoptRec
          THEN \* only metadata moved: retry against the fresh word
               /\ w' = w /\ adoptPhase' = "loaded" /\ adoptObs' = w /\ crash' = crash
          ELSE /\ w' = w /\ adoptPhase' = "idle" /\ adoptObs' = adoptObs /\ crash' = "adoption-lost"
    /\ UNCHANGED <<adoptRec, mbox, retPhase, retCpu, retGen, relPhase, relGen, lanePhase, laneSnap,
                   migPhase, migGen, migCpu>>

(* ---- lane claim: an observed Blocked word, exact CAS later ---- *)

LaneObserve ==
    /\ lanePhase = "idle" /\ w.st = Blocked /\ ~w.arm
    /\ lanePhase' = "observed" /\ laneSnap' = w
    /\ UNCHANGED <<w, retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, migPhase, migGen, migCpu, crash>>

LaneCas(c) ==
    /\ lanePhase = "observed"
    /\ lanePhase' = "idle" /\ laneSnap' = laneSnap
    /\ IF w = laneSnap /\ CanAdvance
          THEN w' = Fresh(Running, c)
          ELSE w' = w \* the lane declines and the ordinary wake finishes the turn
    /\ UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs,
                   mbox, migPhase, migGen, migCpu, crash>>

(* ---- terminal withdrawal: Retiring, then the exact completion CAS ---- *)

RetireStart ==
    /\ retPhase = "idle" /\ relPhase = "idle"
    /\ w.st \notin Terminal \cup {Dormant} /\ CanAdvanceTwice
    /\ w' = Fresh(Retiring, w.cpu)
    /\ retPhase' = "mid" /\ retCpu' = w.cpu /\ retGen' = w.gen + 1
    /\ UNCHANGED <<relPhase, relGen, adoptPhase, adoptRec, adoptObs, mbox, lanePhase, laneSnap,
                   migPhase, migGen, migCpu, crash>>

RetireFinish ==
    /\ retPhase = "mid"
    /\ retPhase' = "idle"
    /\ IF w = RetiringWord(retCpu, retGen)
          THEN /\ w' = Fresh(Retired, NoCpu) /\ crash' = crash
          ELSE /\ w' = w /\ crash' = "retire-completion-lost"
    /\ UNCHANGED <<retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs, mbox, lanePhase,
                   laneSnap, migPhase, migGen, migCpu>>

ReleaseRead ==
    /\ relPhase = "idle" /\ retPhase = "idle" /\ w.st = Retired /\ CanAdvance
    /\ relPhase' = "mid" /\ relGen' = w.gen
    /\ UNCHANGED <<w, retPhase, retCpu, retGen, adoptPhase, adoptRec, adoptObs, mbox, lanePhase,
                   laneSnap, migPhase, migGen, migCpu, crash>>

ReleaseCas ==
    /\ relPhase = "mid"
    /\ relPhase' = "idle"
    /\ IF w = RetiredWord(relGen)
          THEN /\ w' = Fresh(Dormant, NoCpu) /\ crash' = crash
          ELSE /\ w' = w /\ crash' = "release-raced"
    /\ UNCHANGED <<relGen, retPhase, retCpu, retGen, adoptPhase, adoptRec, adoptObs, mbox, lanePhase,
                   laneSnap, migPhase, migGen, migCpu>>

(* ---- in-place metadata writers: same state, CPU and generation ---- *)

MetaUnchanged ==
    UNCHANGED <<retPhase, retCpu, retGen, relPhase, relGen, adoptPhase, adoptRec, adoptObs, mbox,
                lanePhase, laneSnap, migPhase, migGen, migCpu, crash>>

\* set_runnable: current_return.publish_frame (ready) and wait::set_blocked.
SetRunnable(v) ==
    /\ Editable
    /\ w' = [w EXCEPT !.run = v]
    /\ MetaUnchanged

SetWaitArmed(a) ==
    /\ Editable
    /\ w' = [w EXCEPT !.arm = a]
    /\ MetaUnchanged

SetWaitReason(r) ==
    /\ Editable
    /\ w' = [w EXCEPT !.rsn = r]
    /\ MetaUnchanged

ClearWaitArm ==
    /\ Editable
    /\ w' = [w EXCEPT !.arm = FALSE, !.rsn = 0]
    /\ MetaUnchanged

PublishWaitArm ==
    /\ w.st = Running /\ w.run
    /\ w' = [w EXCEPT !.arm = TRUE, !.rsn = 1]
    /\ MetaUnchanged

CommitWait ==
    /\ w.st = Running /\ w.run /\ w.arm
    /\ w' = [w EXCEPT !.run = FALSE, !.arm = FALSE]
    /\ MetaUnchanged

WakeWait ==
    /\ Editable
    /\ w' = [w EXCEPT !.run = (w.run \/ w.st \in RunnableStates), !.arm = FALSE, !.rsn = 0]
    /\ MetaUnchanged

Next ==
    \/ Step(Admit)
    \/ \E c \in Cpus : Step(LocalWake(c))
    \/ \E c \in Cpus : Step(RemoteWake(c))
    \/ \E c \in Cpus : Step(DirectWake(c))
    \/ Step(DirectToLocal)
    \/ Step(DirectRollback)
    \/ Step(Claim)
    \/ Step(Block)
    \/ \E c \in Cpus : Step(MigrateStart(c))
    \/ Step(MigratePublish)
    \/ Step(AdoptLoad)
    \/ Step(AdoptCas)
    \/ Step(LaneObserve)
    \/ \E c \in Cpus : Step(LaneCas(c))
    \/ Step(RetireStart)
    \/ Step(RetireFinish)
    \/ Step(ReleaseRead)
    \/ Step(ReleaseCas)
    \/ \E v \in BOOLEAN : Step(SetRunnable(v))
    \/ \E a \in BOOLEAN : Step(SetWaitArmed(a))
    \/ \E r \in 0..1 : Step(SetWaitReason(r))
    \/ Step(ClearWaitArm)
    \/ Step(PublishWaitArm)
    \/ Step(CommitWait)
    \/ Step(WakeWait)
    \* A completed lifecycle at the generation bound stutters.
    \/ (UNCHANGED vars)

Spec == Init /\ [][Next]_vars

(***************************************************************************
Safety.
***************************************************************************)

\* A terminal word is exactly the word its consumer will compare against: no
\* run intent, no wait arm, no reason. Only the exact retire completion and slot
\* release ever change it.
TerminalWordIsCanonical ==
    /\ w.st = Retiring => w = RetiringWord(w.cpu, w.gen)
    /\ w.st = Retired => w = RetiredWord(w.gen)

\* The exact-value consumers, the mailbox, the migration publication and the
\* record accounting never reach a panic site.
NoPanic == crash = "none"

\* Retire completion cannot lose while its Retiring word is published.
RetireCompletionCannotLose ==
    retPhase = "mid" => (w.st = Retiring /\ w.gen = retGen => w = RetiringWord(retCpu, retGen))

\* Generations only move forward, one custody transition at a time.
GenerationMonotonic == w.gen >= prevGen /\ w.gen <= prevGen + 1

\* A Dormant slot is settled by the exact admission CAS, so no writer leaves run
\* intent or wait metadata on it.
DormantWordIsClean == w.st = Dormant => (~w.run /\ ~w.arm /\ w.rsn = 0 /\ w.cpu = NoCpu)

====
