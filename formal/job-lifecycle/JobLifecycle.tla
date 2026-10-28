-------------------- MODULE JobLifecycle --------------------
EXTENDS Naturals

(***************************************************************************
Models the kernel job tree: group lifetime and membership of processes.

Concrete owner:
  * kernel/ps/src/multitask/process_table/jobs.rs
        (job_create_child, job_kill_begin, job_collect_members,
         release_dead_jobs, resolve_new_member_job)
  * kernel/ps/src/multitask/process_table/spawn.rs (publish_inner)

A job slot is recycled with a new generation. Membership lives in the process
objects and changes under the same lock as the slots, so these steps are
atomic. A process joins one job for life. Killing a job marks it and every
descendant, which refuses new members and children; the slot may be recycled
only when no process object, exiting or not, still names it and no child job
remains.
***************************************************************************)

Jobs == 0..2
Procs == {"p1", "p2"}
Root == 0
None == 99
MaxGen == 2

VARIABLES jinuse, jkilling, jparent, jgen, pstate, pjob, pjgen, joinedKilling

vars == <<jinuse, jkilling, jparent, jgen, pstate, pjob, pjgen, joinedKilling>>

TypeOK ==
    /\ jinuse \in [Jobs -> BOOLEAN]
    /\ jkilling \in [Jobs -> BOOLEAN]
    /\ jparent \in [Jobs -> Jobs \cup {None}]
    /\ jgen \in [Jobs -> 1..MaxGen]
    /\ pstate \in [Procs -> {"absent", "live", "exiting"}]
    /\ pjob \in [Procs -> Jobs \cup {None}]
    /\ pjgen \in [Procs -> 0..MaxGen]
    /\ joinedKilling \in [Procs -> BOOLEAN]

Init ==
    /\ jinuse = [j \in Jobs |-> j = Root]
    /\ jkilling = [j \in Jobs |-> FALSE]
    /\ jparent = [j \in Jobs |-> None]
    /\ jgen = [j \in Jobs |-> 1]
    /\ pstate = [p \in Procs |-> "absent"]
    /\ pjob = [p \in Procs |-> None]
    /\ pjgen = [p \in Procs |-> 0]
    /\ joinedKilling = [p \in Procs |-> FALSE]

\* k is j or lies below it (the model's depth is at most two).
InSubtree(j, k) ==
    \/ k = j
    \/ jparent[k] = j
    \/ (jparent[k] # None /\ jparent[jparent[k]] = j)

CreateChild(parent, j) ==
    /\ ~jinuse[j]
    /\ jinuse[parent]
    /\ ~jkilling[parent]
    /\ jinuse' = [jinuse EXCEPT ![j] = TRUE]
    /\ jparent' = [jparent EXCEPT ![j] = parent]
    /\ jkilling' = [jkilling EXCEPT ![j] = FALSE]
    /\ UNCHANGED <<jgen, pstate, pjob, pjgen, joinedKilling>>

Publish(p, j) ==
    /\ pstate[p] = "absent"
    /\ jinuse[j]
    /\ ~jkilling[j] \* publish guard
    /\ pstate' = [pstate EXCEPT ![p] = "live"]
    /\ pjob' = [pjob EXCEPT ![p] = j]
    /\ pjgen' = [pjgen EXCEPT ![p] = jgen[j]]
    /\ joinedKilling' = [joinedKilling EXCEPT ![p] = jkilling[j]]
    /\ UNCHANGED <<jinuse, jkilling, jparent, jgen>>

Kill(j) ==
    /\ j # Root
    /\ jinuse[j]
    /\ jkilling' = [k \in Jobs |-> IF jinuse[k] /\ InSubtree(j, k) THEN TRUE ELSE jkilling[k]]
    /\ UNCHANGED <<jinuse, jparent, jgen, pstate, pjob, pjgen, joinedKilling>>

Exit(p) ==
    /\ pstate[p] = "live"
    /\ pstate' = [pstate EXCEPT ![p] = "exiting"]
    /\ UNCHANGED <<jinuse, jkilling, jparent, jgen, pjob, pjgen, joinedKilling>>

Reap(p) ==
    /\ pstate[p] = "exiting"
    /\ pstate' = [pstate EXCEPT ![p] = "absent"]
    /\ pjob' = [pjob EXCEPT ![p] = None]
    /\ pjgen' = [pjgen EXCEPT ![p] = 0]
    /\ joinedKilling' = [joinedKilling EXCEPT ![p] = FALSE]
    /\ UNCHANGED <<jinuse, jkilling, jparent, jgen>>

Release(j) ==
    /\ j # Root
    /\ jinuse[j]
    /\ jkilling[j]
    /\ \A k \in Jobs: ~(jinuse[k] /\ jparent[k] = j) \* no child job
    /\ \A p \in Procs: ~(pstate[p] # "absent" /\ pjob[p] = j) \* no member object
    /\ jgen[j] < MaxGen
    /\ jinuse' = [jinuse EXCEPT ![j] = FALSE]
    /\ jparent' = [jparent EXCEPT ![j] = None]
    /\ jkilling' = [jkilling EXCEPT ![j] = FALSE]
    /\ jgen' = [jgen EXCEPT ![j] = jgen[j] + 1]
    /\ UNCHANGED <<pstate, pjob, pjgen, joinedKilling>>

Next ==
    \/ \E parent \in Jobs, j \in Jobs: CreateChild(parent, j)
    \/ \E p \in Procs, j \in Jobs: Publish(p, j)
    \/ \E j \in Jobs: Kill(j) \/ Release(j)
    \/ \E p \in Procs: Exit(p) \/ Reap(p)
    \/ UNCHANGED vars \* the bounded generation space is exhausted

Spec == Init /\ [][Next]_vars

\* A process object names a live job slot of the generation it joined, so a
\* recycled slot never aliases a member.
MemberJobIsLive ==
    \A p \in Procs:
        pstate[p] # "absent" =>
            /\ jinuse[pjob[p]]
            /\ jgen[pjob[p]] = pjgen[p]

\* Killing marks the whole subtree: no live child hangs under a killing job.
KillingClosedUnderChildren ==
    \A k \in Jobs:
        (jinuse[k] /\ jparent[k] # None /\ jkilling[jparent[k]]) => jkilling[k]

\* No process ever joined a job that was being killed.
NoMemberJoinsKillingJob == \A p \in Procs: ~joinedKilling[p]

\* The root job is never killed and never recycled.
RootIsPermanent == jinuse[Root] /\ ~jkilling[Root] /\ jgen[Root] = 1

\* A job's parent outlives it.
ParentOutlivesChild ==
    \A k \in Jobs: (jinuse[k] /\ k # Root) => jinuse[jparent[k]]

=============================================================================
