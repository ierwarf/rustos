-------------------- MODULE OomTerminationOwner --------------------
EXTENDS Naturals

(***************************************************************************
Models two terminators of one process, the root supervisor and the ring0
memory-pressure watchdog, racing to end it.

Concrete owners:
  * kernel/ps/src/multitask/current/group_exit.rs  (request_marked)
  * kernel/ps/src/multitask/current.rs             (terminate_user_process_once)
  * kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs
        (syscall_linux_rustos_rootd_terminate_broker,
         terminate_process_once_and_finalize, finalize_terminated_process)

A termination request that finds one already queued joins it and waits for the
same exit. Only the terminator that published the request may run the
once-per-process follow-up (service-handle revocation, endpoint cleanup, exit
status, lifecycle exit record, parent SIGCHLD); a second run records the exit
twice.
***************************************************************************)

Who == {"rootd", "watchdog"}

Idle == "idle"
Publisher == "publisher"
Joiner == "joiner"
Done == "done"

VARIABLES queued, owner, finalized, pc

vars == <<queued, owner, finalized, pc>>

TypeOK ==
    /\ queued \in BOOLEAN
    /\ owner \in Who \cup {"none"}
    /\ finalized \in 0..2
    /\ pc \in [Who -> {Idle, Publisher, Joiner, Done}]

Init ==
    /\ queued = FALSE
    /\ owner = "none"
    /\ finalized = 0
    /\ pc = [who \in Who |-> Idle]

\* The request: publish if none is queued, otherwise join the queued one.
Request(who) ==
    /\ pc[who] = Idle
    /\ IF ~queued
          THEN /\ queued' = TRUE
               /\ owner' = who
               /\ pc' = [pc EXCEPT ![who] = Publisher]
          ELSE /\ queued' = queued
               /\ owner' = owner
               /\ pc' = [pc EXCEPT ![who] = Joiner]
    /\ UNCHANGED finalized

\* The publisher's once-per-process follow-up.
Finalize(who) ==
    /\ pc[who] = Publisher
    /\ finalized' = finalized + 1
    /\ pc' = [pc EXCEPT ![who] = Done]
    /\ UNCHANGED <<queued, owner>>

\* A joiner waits for the exit and performs no follow-up.
Join(who) ==
    /\ pc[who] = Joiner
    /\ pc' = [pc EXCEPT ![who] = Done]
    /\ UNCHANGED <<queued, owner, finalized>>

Finished ==
    /\ \A who \in Who: pc[who] = Done
    /\ UNCHANGED vars

Next ==
    \/ \E who \in Who: Request(who) \/ Finalize(who) \/ Join(who)
    \/ Finished

Spec == Init /\ [][Next]_vars

\* The follow-up runs at most once per process, whoever wins the race.
AtMostOneFinalization == finalized <= 1

\* A follow-up only follows a published request.
FinalizedMeansPublished == finalized > 0 => queued

\* When every terminator is done, exactly one follow-up ran.
ExactlyOneFinalizationWhenFinished ==
    (\A who \in Who: pc[who] = Done) => finalized = 1

=============================================================================
