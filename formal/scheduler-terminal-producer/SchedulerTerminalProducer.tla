------------------ MODULE SchedulerTerminalProducer ------------------
EXTENDS Naturals

(***************************************************************************
Owner: the exact terminal producer loan of one task slot, from the stop request
through withdrawal, deferral and final publication.

Concrete owners:
  * scheduler/retirement_cleanup/producer.rs  Producer begin, claim,
                      wait_for_executor, park_unwithdrawn, defer, publish
  * scheduler/terminal_quarantine.rs          begin, claim, withdraw
  * scheduler/terminal_publication.rs         guarded, drain_guarded
  * scheduler/runqueue.rs                     retire_slot (ClaimInFlight)

A deferred stop has three exact origins, and the housekeeping drain
(`drain_guarded`) claims every one of them:
  * deferred   : the stop was withdrawn (lifecycle retired) and quiescence
                 failed, so the SAME producer was deferred;
  * waiting    : the stop was reserved while a kernel executor held a resume
                 loan, nothing withdrawn;
  * parked     : the stop was reserved while another CPU's dispatch claim of
                 the task was in flight (`ClaimInFlight`), nothing withdrawn.
The parked origin is the one the drain's lifecycle assertion forgot: a claimed
parked producer has a live lifecycle and no executor wait, and the assertion
panicked the kernel ("deferred retirement lost its terminal lifecycle or kernel
resume loan", 4-vCPU boot of 2026-11-09). The code now carries the origin as one
typed value (`Producer::origin`, `Origin::{Withdrawn, ExecutorWait, Parked}`)
and the drain asserts that the lifecycle is retired exactly for a withdrawn
origin. A claimant that gives up untouched (for example
`terminal_quarantine::claim` on a binding mismatch) restores the exact origin
phase (`Rollback`).

Actors: the dispatch claimer (a delayed CPU between its owner CAS, its install
and its publication of Current), the stop requester, the housekeeping drain and
the kernel executor whose resume loan can be held arbitrarily long.
***************************************************************************)

Phases == {"vacant", "reserved", "parked", "waiting", "deferred", "claimed", "effects", "published"}
Holders == {"none", "req", "hk"}
Origins == {"none", "parked", "waiting", "deferred"}
Claims == {"idle", "claimed", "installed", "published"}
Owners == {"local", "running", "retired"}

VARIABLES owner, claim, role, producer, holder, from, lifecycle, execLoan,
          requested, done, crash

vars == <<owner, claim, role, producer, holder, from, lifecycle, execLoan,
          requested, done, crash>>

TypeOK ==
    /\ owner \in Owners
    /\ claim \in Claims
    /\ role \in BOOLEAN
    /\ producer \in Phases
    /\ holder \in Holders
    /\ from \in Origins
    /\ lifecycle \in BOOLEAN
    /\ execLoan \in BOOLEAN
    /\ requested \in BOOLEAN
    /\ done \in BOOLEAN
    /\ crash \in {"none", "fatal-install", "housekeeping-assert"}

Init ==
    /\ owner = "local" /\ claim = "idle" /\ role = FALSE
    /\ producer = "vacant" /\ holder = "none" /\ from = "none"
    /\ lifecycle = FALSE
    /\ execLoan \in BOOLEAN
    /\ requested = FALSE /\ done = FALSE
    /\ crash = "none"

InFlight == owner = "running" /\ ~role

(* ---- the dispatch claimer: owner CAS, install, then publication ---- *)

ClaimCas ==
    /\ claim = "idle" /\ owner = "local"
    /\ owner' = "running" /\ claim' = "claimed"
    /\ UNCHANGED <<role, producer, holder, from, lifecycle, execLoan, requested, done, crash>>

Install ==
    /\ claim = "claimed"
    /\ IF owner = "running"
          THEN /\ claim' = "installed" /\ crash' = crash
          ELSE /\ claim' = claim /\ crash' = "fatal-install"
    /\ UNCHANGED <<owner, role, producer, holder, from, lifecycle, execLoan, requested, done>>

PublishCurrent ==
    /\ claim = "installed"
    /\ claim' = "published" /\ role' = TRUE
    /\ UNCHANGED <<owner, producer, holder, from, lifecycle, execLoan, requested, done, crash>>

(* ---- the kernel executor's resume loan ---- *)

ExecDrain ==
    /\ execLoan /\ execLoan' = FALSE
    /\ UNCHANGED <<owner, claim, role, producer, holder, from, lifecycle, requested, done, crash>>

(* ---- the stop requester ---- *)

Begin ==
    /\ ~requested /\ producer = "vacant"
    /\ requested' = TRUE /\ producer' = "reserved" /\ holder' = "req"
    /\ UNCHANGED <<owner, claim, role, from, lifecycle, execLoan, done, crash>>

\* Producer::wait_for_executor: keep the first stop request, withdraw nothing.
WaitExecutor ==
    /\ holder = "req" /\ producer = "reserved" /\ execLoan
    /\ producer' = "waiting" /\ holder' = "none"
    /\ UNCHANGED <<owner, claim, role, from, lifecycle, execLoan, requested, done, crash>>

(* ---- withdrawal by whoever holds the producer ---- *)

Withdraw(h) ==
    /\ holder = h /\ producer \in {"reserved", "claimed"}
    /\ IF owner = "retired"
          THEN \* retire_slot returns Retired: the original withdrawal already happened
               /\ producer' = "effects" /\ UNCHANGED <<owner, lifecycle, holder, from>>
       ELSE IF InFlight
          THEN \* park_unwithdrawn: nothing withdrawn, the request is kept
               /\ producer' = "parked" /\ holder' = "none" /\ from' = "none"
               /\ UNCHANGED <<owner, lifecycle>>
          ELSE /\ owner' = "retired" /\ lifecycle' = TRUE
               /\ producer' = "effects" /\ UNCHANGED <<holder, from>>
    /\ UNCHANGED <<claim, role, execLoan, requested, done, crash>>

\* After the effects: quiescence fails and the SAME producer is deferred, or the
\* terminal publication completes.
Quiesce ==
    /\ producer = "effects"
    /\ \/ /\ producer' = "deferred" /\ holder' = "none" /\ UNCHANGED done
       \/ /\ producer' = "published" /\ holder' = "none" /\ done' = TRUE
    /\ UNCHANGED <<owner, claim, role, from, lifecycle, execLoan, requested, crash>>

(* ---- claim of a deferred producer, by the requester or by housekeeping ---- *)

Claim(h) ==
    /\ holder = "none" /\ producer \in {"parked", "waiting", "deferred"}
    /\ (producer = "waiting" => ~execLoan)
    /\ from' = producer /\ producer' = "claimed" /\ holder' = h
    \* drain_guarded: lifecycle retired <=> the origin is withdrawn ("deferred").
    /\ crash' = IF h = "hk" /\ (lifecycle # (producer = "deferred"))
                   THEN "housekeeping-assert" ELSE crash
    /\ UNCHANGED <<owner, claim, role, lifecycle, execLoan, requested, done>>

\* Drop of a claimed producer that withdrew nothing under this claim: the exact
\* origin phase comes back, never another one.
Rollback(h) ==
    /\ holder = h /\ producer = "claimed" /\ from # "none"
    /\ producer' = from /\ holder' = "none" /\ from' = "none"
    /\ UNCHANGED <<owner, claim, role, lifecycle, execLoan, requested, done, crash>>

Next ==
    \/ ClaimCas \/ Install \/ PublishCurrent \/ ExecDrain
    \/ Begin \/ WaitExecutor
    \/ Withdraw("req") \/ Withdraw("hk")
    \/ Quiesce
    \/ Claim("req") \/ Claim("hk")
    \/ Rollback("req") \/ Rollback("hk")
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars

(***************************************************************************
Safety.
***************************************************************************)

\* No fatal install and no housekeeping assertion.
NoPanic == crash = "none"

\* A parked or waiting producer has withdrawn nothing.
ParkedAndWaitingWithdrewNothing ==
    producer \in {"parked", "waiting"} => (~lifecycle /\ owner # "retired")

\* A stop request is never dropped before its publication.
StopRequestKept == (requested /\ ~done) => producer # "vacant"

\* A retired owner means the stop holds a withdrawn producer.
RetiredOwnerHasStop == owner = "retired" => (lifecycle /\ requested)

\* A deferred (withdrawn) stop has retired the lifecycle and the owner word.
DeferredMeansWithdrawn ==
    producer = "deferred" => (lifecycle /\ owner = "retired")

\* A published terminal stop retired the lifecycle.
PublishedMeansRetired == producer = "published" => lifecycle

\* Claimed, effects and reserved producers have exactly one holder.
HeldProducerHasHolder ==
    producer \in {"reserved", "claimed", "effects"} <=> holder # "none"

=============================================================================
