-------------------- MODULE SchedulerTerminalClaim --------------------
EXTENDS Naturals

(***************************************************************************
Models the window between one CPU's dispatch claim of a task and that CPU's
publication of the task as its Current, against a stop requested from another
CPU.

Concrete owners:
  * kernel/ps/src/multitask/scheduler/dispatch_claim.rs      (claim)
  * kernel/ps/src/multitask/scheduler/runqueue.rs            (retire_slot)
  * kernel/ps/src/multitask/scheduler/return_architecture.rs (capture_checked)
  * kernel/ps/src/multitask/scheduler/terminal_quarantine.rs (withdraw)
  * kernel/ps/src/multitask/scheduler/terminal_publication.rs (guarded)
  * libs/rustos-scheduler-frame-gate (terminal_withdraw_must_wait)

The claim is the owner-word CAS Local -> Running. The claiming CPU then
installs the claimed architecture, which re-checks that the owner word is still
Running, and later publishes the task as its Current. A remote withdrawal that
retires the owner word between the CAS and the publication makes that install
refuse a claim it owns, which the IRQ path treats as fatal.

A withdrawal therefore retires the owner word in one step with its own check:
a Running owner with no published execution role is an in-flight claim, and the
stop parks untouched until the role is published. Once the role is published
the same retirement is the normal remote stop of an executing task.
***************************************************************************)

OwnerLocal == "local"
OwnerRunning == "running"
OwnerRetired == "retired"

ClaimIdle == "idle"
ClaimClaimed == "claimed"
ClaimInstalled == "installed"
ClaimPublished == "published"

StopNone == "none"
StopReserved == "reserved"
StopParked == "parked"
StopWithdrawn == "withdrawn"

VARIABLES owner, claim, role, stop, fatal

vars == <<owner, claim, role, stop, fatal>>

TypeOK ==
    /\ owner \in {OwnerLocal, OwnerRunning, OwnerRetired}
    /\ claim \in {ClaimIdle, ClaimClaimed, ClaimInstalled, ClaimPublished}
    /\ role \in BOOLEAN
    /\ stop \in {StopNone, StopReserved, StopParked, StopWithdrawn}
    /\ fatal \in BOOLEAN

Init ==
    /\ owner = OwnerLocal
    /\ claim = ClaimIdle
    /\ role = FALSE
    /\ stop = StopNone
    /\ fatal = FALSE

\* The claimer's owner CAS. It cannot win once the owner word has been retired.
ClaimCas ==
    /\ claim = ClaimIdle
    /\ owner = OwnerLocal
    /\ owner' = OwnerRunning
    /\ claim' = ClaimClaimed
    /\ UNCHANGED <<role, stop, fatal>>

\* The claimer's install: capture_checked re-reads the owner word. A retired
\* owner is refused, and the IRQ path stops the kernel on that refusal.
Install ==
    /\ claim = ClaimClaimed
    /\ IF owner = OwnerRunning
          THEN /\ claim' = ClaimInstalled
               /\ fatal' = fatal
          ELSE /\ claim' = claim
               /\ fatal' = TRUE
    /\ UNCHANGED <<owner, role, stop>>

\* The switch publishes the task as the claimer's Current.
Publish ==
    /\ claim = ClaimInstalled
    /\ claim' = ClaimPublished
    /\ role' = TRUE
    /\ UNCHANGED <<owner, stop, fatal>>

BeginStop ==
    /\ stop = StopNone
    /\ stop' = StopReserved
    /\ UNCHANGED <<owner, claim, role, fatal>>

InFlight == owner = OwnerRunning /\ ~role

\* One step: the retirement CAS and its in-flight check.
Withdraw ==
    /\ stop \in {StopReserved, StopParked}
    /\ IF InFlight
          THEN /\ stop' = StopParked
               /\ owner' = owner
          ELSE /\ stop' = StopWithdrawn
               /\ owner' = OwnerRetired
    /\ UNCHANGED <<claim, role, fatal>>

Done ==
    /\ stop = StopWithdrawn
    /\ UNCHANGED vars

Next == ClaimCas \/ Install \/ Publish \/ BeginStop \/ Withdraw \/ Done

Spec == Init /\ [][Next]_vars

\* The installing CPU is never handed a claim whose owner word was retired.
NoFatalInstall == ~fatal

\* Retirement never happens while a claim is between its CAS and its install.
NoRetireUnderClaim ==
    owner = OwnerRetired => claim \in {ClaimIdle, ClaimPublished}

\* A parked stop has not touched the owner word: it parks, it does not retire.
ParkedStopHasNotRetiredTheOwner ==
    stop = StopParked => owner = OwnerRunning

\* A completed withdrawal means the owner word really is retired, so a later
\* claim cannot resurrect a task whose stop already finished.
WithdrawnMeansRetired ==
    stop = StopWithdrawn => owner = OwnerRetired

=============================================================================
