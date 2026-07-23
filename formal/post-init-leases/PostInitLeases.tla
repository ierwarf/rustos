------------------------------ MODULE PostInitLeases ------------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models rootd post-init lease admission and restart budgets. Initd owns the
netd and sessiond launch reports, while the live sessiond/runtimed service owns
the uiserver report. `InitialLaunch` is the kernel-owned deferred-spawn
provenance: source admission additionally requires the exact declared
executable path before `ReportReadiness`. A capability is granted only to the
exact running PID whose complete reporter chain terminates at the live initd
lease.
An attempted re-registration by any other actor is a stateful rejection: it
may be observed for diagnostics, but cannot change the live PID, reporter, or
capability binding.
***************************************************************************)

CONSTANTS Services, Supervisors, Pids, InitialRestartBudget

NoPid == 0
NoSupervisor == "none"
NoService == "none-service"
NoActor == "none-actor"
InitdSupervisor == "initd"
SessiondSupervisor == "sessiond"
Intruder == "intruder"
Actors == Supervisors \cup {Intruder}

Unlaunched == "unlaunched"
PendingReport == "pending-report"
Running == "running"
Exited == "exited"
Failed == "failed"

SupervisorFor(s) ==
    IF s \in {"netd", "sessiond"} THEN InitdSupervisor ELSE SessiondSupervisor

Dependents(s) == IF s = "sessiond" THEN {"uiserver"} ELSE {}
Cascade(s) == {s} \cup Dependents(s)

VARIABLES leaseState,
          leasePid,
          reportedBy,
          capabilityPid,
          restartRemaining,
          restartCount,
          supervisorAlive,
          issuedPids,
          lastRebindService,
          lastRebindActor,
          lastRebindLeasePid,
          lastRebindReporter,
          lastRebindCapabilityPid

vars == <<leaseState, leasePid, reportedBy, capabilityPid, restartRemaining,
          restartCount, supervisorAlive, issuedPids, lastRebindService, lastRebindActor,
          lastRebindLeasePid, lastRebindReporter, lastRebindCapabilityPid>>

Init ==
    /\ leaseState = [s \in Services |-> Unlaunched]
    /\ leasePid = [s \in Services |-> NoPid]
    /\ reportedBy = [s \in Services |-> NoSupervisor]
    /\ capabilityPid = [s \in Services |-> NoPid]
    /\ restartRemaining = [s \in Services |-> InitialRestartBudget]
    /\ restartCount = [s \in Services |-> 0]
    /\ supervisorAlive = [supervisor \in Supervisors |-> supervisor = InitdSupervisor]
    /\ issuedPids = {}
    /\ lastRebindService = NoService
    /\ lastRebindActor = NoActor
    /\ lastRebindLeasePid = NoPid
    /\ lastRebindReporter = NoSupervisor
    /\ lastRebindCapabilityPid = NoPid

ClearRebindAttempt ==
    /\ lastRebindService' = NoService
    /\ lastRebindActor' = NoActor
    /\ lastRebindLeasePid' = NoPid
    /\ lastRebindReporter' = NoSupervisor
    /\ lastRebindCapabilityPid' = NoPid

InitialLaunch(s, p, supervisor) ==
    /\ s \in Services
    /\ p \in Pids \ issuedPids
    /\ supervisor = SupervisorFor(s)
    /\ supervisorAlive[supervisor]
    /\ leaseState[s] = Unlaunched
    /\ leaseState' = [leaseState EXCEPT ![s] = PendingReport]
    /\ leasePid' = [leasePid EXCEPT ![s] = p]
    /\ reportedBy' = [reportedBy EXCEPT ![s] = NoSupervisor]
    /\ issuedPids' = issuedPids \cup {p}
    /\ ClearRebindAttempt
    /\ UNCHANGED <<capabilityPid, restartRemaining, restartCount, supervisorAlive>>

ReportReadiness(s, p, supervisor) ==
    /\ s \in Services
    /\ p \in Pids
    /\ supervisor = SupervisorFor(s)
    /\ supervisorAlive[supervisor]
    /\ p \in issuedPids
    /\ leaseState[s] = PendingReport
    /\ leasePid[s] = p
    /\ reportedBy[s] = NoSupervisor
    /\ leaseState' = [leaseState EXCEPT ![s] = Running]
    /\ reportedBy' = [reportedBy EXCEPT ![s] = supervisor]
    /\ supervisorAlive' =
        IF s = "sessiond"
        THEN [supervisorAlive EXCEPT ![SessiondSupervisor] = TRUE]
        ELSE supervisorAlive
    /\ ClearRebindAttempt
    /\ UNCHANGED <<leasePid, capabilityPid, restartRemaining, restartCount, issuedPids>>

GrantCapability(s, p) ==
    /\ s \in Services
    /\ p \in Pids
    /\ leaseState[s] = Running
    /\ leasePid[s] = p
    /\ reportedBy[s] = SupervisorFor(s)
    /\ supervisorAlive[reportedBy[s]]
    /\ capabilityPid[s] = NoPid
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = p]
    /\ ClearRebindAttempt
    /\ UNCHANGED <<leaseState, leasePid, reportedBy, restartRemaining, supervisorAlive,
                  restartCount, issuedPids>>

Exit(s) ==
    /\ s \in Services
    /\ leaseState[s] \in {PendingReport, Running}
    /\ leaseState' = [service \in Services |->
        IF service \in Cascade(s) /\ leaseState[service] \in {PendingReport, Running}
        THEN Exited ELSE leaseState[service]]
    /\ leasePid' = [service \in Services |->
        IF service \in Cascade(s) /\ leaseState[service] \in {PendingReport, Running}
        THEN NoPid ELSE leasePid[service]]
    /\ reportedBy' = [service \in Services |->
        IF service \in Cascade(s) /\ leaseState[service] \in {PendingReport, Running}
        THEN NoSupervisor ELSE reportedBy[service]]
    /\ capabilityPid' = [service \in Services |->
        IF service \in Cascade(s) /\ leaseState[service] \in {PendingReport, Running}
        THEN NoPid ELSE capabilityPid[service]]
    /\ supervisorAlive' =
        IF s = "sessiond"
        THEN [supervisorAlive EXCEPT ![SessiondSupervisor] = FALSE]
        ELSE supervisorAlive
    /\ ClearRebindAttempt
    /\ UNCHANGED <<restartRemaining, restartCount, issuedPids>>

Restart(s, p, supervisor) ==
    /\ s \in Services
    /\ p \in Pids \ issuedPids
    /\ supervisor = SupervisorFor(s)
    /\ supervisorAlive[supervisor]
    /\ leaseState[s] = Exited
    /\ restartRemaining[s] > 0
    /\ leaseState' = [leaseState EXCEPT ![s] = PendingReport]
    /\ leasePid' = [leasePid EXCEPT ![s] = p]
    /\ reportedBy' = [reportedBy EXCEPT ![s] = NoSupervisor]
    /\ restartRemaining' = [restartRemaining EXCEPT ![s] = @ - 1]
    /\ restartCount' = [restartCount EXCEPT ![s] = @ + 1]
    /\ issuedPids' = issuedPids \cup {p}
    /\ ClearRebindAttempt
    /\ UNCHANGED <<capabilityPid, supervisorAlive>>

Exhaust(s) ==
    /\ s \in Services
    /\ leaseState[s] = Exited
    /\ restartRemaining[s] = 0
    /\ leaseState' = [leaseState EXCEPT ![s] = Failed]
    /\ ClearRebindAttempt
    /\ UNCHANGED <<leasePid, reportedBy, capabilityPid, restartRemaining, supervisorAlive,
                  restartCount, issuedPids>>

SupervisorExit(supervisor) ==
    \* initd is the external root of this reporter graph. Its observed exit
    \* synchronously revokes sessiond and the UI descendant in the same rootd
    \* turn; a later sessiond lifecycle record is not needed for authority.
    /\ supervisor = InitdSupervisor
    /\ supervisorAlive[supervisor]
    /\ supervisorAlive' = [role \in Supervisors |-> FALSE]
    /\ leaseState' = [s \in Services |->
        IF leaseState[s] \in {PendingReport, Running}
        THEN Exited ELSE leaseState[s]]
    /\ leasePid' = [s \in Services |->
        IF leaseState[s] \in {PendingReport, Running}
        THEN NoPid ELSE leasePid[s]]
    /\ reportedBy' = [s \in Services |->
        IF leaseState[s] \in {PendingReport, Running}
        THEN NoSupervisor ELSE reportedBy[s]]
    /\ capabilityPid' = [s \in Services |->
        IF leaseState[s] \in {PendingReport, Running}
        THEN NoPid ELSE capabilityPid[s]]
    /\ ClearRebindAttempt
    /\ UNCHANGED <<restartRemaining, restartCount, issuedPids>>

\* This corresponds to rootd rejecting a readiness registration when a live
\* lease has the same exact child PID but a different `reporter_pid` sender.
\* Recording the rejected request makes the preservation guarantee checkable;
\* the authority-bearing fields themselves are intentionally unchanged.
RejectForeignRebind(s, actor) ==
    /\ s \in Services
    /\ actor \in Actors
    /\ leaseState[s] = Running
    /\ actor # reportedBy[s]
    /\ lastRebindService' = s
    /\ lastRebindActor' = actor
    /\ lastRebindLeasePid' = leasePid[s]
    /\ lastRebindReporter' = reportedBy[s]
    /\ lastRebindCapabilityPid' = capabilityPid[s]
    /\ UNCHANGED <<leaseState, leasePid, reportedBy, capabilityPid,
                  restartRemaining, restartCount, supervisorAlive, issuedPids>>

Next ==
    \/ \E s \in Services, p \in Pids, supervisor \in Supervisors :
        InitialLaunch(s, p, supervisor)
    \/ \E s \in Services, p \in Pids, supervisor \in Supervisors :
        ReportReadiness(s, p, supervisor)
    \/ \E s \in Services, p \in Pids : GrantCapability(s, p)
    \/ \E s \in Services : Exit(s)
    \/ \E s \in Services, p \in Pids, supervisor \in Supervisors :
        Restart(s, p, supervisor)
    \/ \E s \in Services : Exhaust(s)
    \/ \E supervisor \in Supervisors : SupervisorExit(supervisor)
    \/ \E s \in Services, actor \in Actors : RejectForeignRebind(s, actor)

TypeOK ==
    /\ Services = {"netd", "sessiond", "uiserver"}
    /\ Supervisors = {InitdSupervisor, SessiondSupervisor}
    /\ Pids \subseteq Nat
    /\ NoPid \notin Pids
    /\ InitialRestartBudget \in Nat
    /\ leaseState \in [Services -> {Unlaunched, PendingReport, Running, Exited, Failed}]
    /\ leasePid \in [Services -> (Pids \cup {NoPid})]
    /\ reportedBy \in [Services -> (Supervisors \cup {NoSupervisor})]
    /\ capabilityPid \in [Services -> (Pids \cup {NoPid})]
    /\ restartRemaining \in [Services -> 0..InitialRestartBudget]
    /\ restartCount \in [Services -> 0..InitialRestartBudget]
    /\ supervisorAlive \in [Supervisors -> BOOLEAN]
    /\ issuedPids \subseteq Pids
    /\ lastRebindService \in Services \cup {NoService}
    /\ lastRebindActor \in Actors \cup {NoActor}
    /\ lastRebindLeasePid \in Pids \cup {NoPid}
    /\ lastRebindReporter \in Supervisors \cup {NoSupervisor}
    /\ lastRebindCapabilityPid \in Pids \cup {NoPid}

PendingLeaseHasNoAuthority ==
    \A s \in Services:
        leaseState[s] = PendingReport =>
            /\ leasePid[s] # NoPid
            /\ reportedBy[s] = NoSupervisor
            /\ capabilityPid[s] = NoPid

RunningLeaseWasReportedByOwner ==
    \A s \in Services:
        leaseState[s] = Running =>
            /\ leasePid[s] # NoPid
            /\ reportedBy[s] = SupervisorFor(s)
            /\ supervisorAlive[reportedBy[s]]
            /\ capabilityPid[s] \in {NoPid, leasePid[s]}

LiveLeaseRequiresLiveSupervisor ==
    \A s \in Services:
        leaseState[s] \in {PendingReport, Running} => supervisorAlive[SupervisorFor(s)]

CapabilityNeedsExactRunningReportedPid ==
    \A s \in Services:
        capabilityPid[s] # NoPid =>
            /\ leaseState[s] = Running
            /\ capabilityPid[s] = leasePid[s]
            /\ reportedBy[s] = SupervisorFor(s)
            /\ supervisorAlive[reportedBy[s]]

ReporterChainEndsAtLiveInitd ==
    /\ leaseState["sessiond"] = Running => supervisorAlive[InitdSupervisor]
    /\ leaseState["uiserver"] = Running =>
        /\ leaseState["sessiond"] = Running
        /\ supervisorAlive[SessiondSupervisor]
        /\ supervisorAlive[InitdSupervisor]

TerminalLeasesClearAuthority ==
    \A s \in Services:
        leaseState[s] \in {Unlaunched, Exited, Failed} =>
            /\ leasePid[s] = NoPid
            /\ reportedBy[s] = NoSupervisor
            /\ capabilityPid[s] = NoPid

RestartBudgetIsConserved ==
    \A s \in Services:
        restartRemaining[s] + restartCount[s] = InitialRestartBudget

FailedLeaseExhaustedBudget ==
    \A s \in Services:
        leaseState[s] = Failed => restartRemaining[s] = 0

AllLiveLeasePidsWereIssued ==
    \A s \in Services:
        /\ leasePid[s] # NoPid => leasePid[s] \in issuedPids
        /\ capabilityPid[s] # NoPid => capabilityPid[s] \in issuedPids

DistinctLiveLeasePids ==
    \A s \in Services:
        \A t \in Services:
            /\ s # t
            /\ leasePid[s] # NoPid
            /\ leasePid[t] # NoPid
            => leasePid[s] # leasePid[t]

RejectedForeignRebindPreservesBinding ==
    lastRebindService # NoService =>
        /\ lastRebindActor # lastRebindReporter
        /\ leaseState[lastRebindService] = Running
        /\ leasePid[lastRebindService] = lastRebindLeasePid
        /\ reportedBy[lastRebindService] = lastRebindReporter
        /\ capabilityPid[lastRebindService] = lastRebindCapabilityPid

=============================================================================
