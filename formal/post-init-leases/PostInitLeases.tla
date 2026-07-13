------------------------------ MODULE PostInitLeases ------------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models rootd post-init lease admission and restart budgets. Initd owns the
netd launch report and runtimed owns the uiserver report. A capability is
granted only to the exact running PID that an authorized supervisor reported.
***************************************************************************)

CONSTANTS Services, Supervisors, Pids, InitialRestartBudget

NoPid == 0
NoSupervisor == "none"
InitdSupervisor == "initd"
RuntimedSupervisor == "runtimed"

Unlaunched == "unlaunched"
PendingReport == "pending-report"
Running == "running"
Exited == "exited"
Failed == "failed"

SupervisorFor(s) ==
    IF s = "netd" THEN InitdSupervisor ELSE RuntimedSupervisor

VARIABLES leaseState,
          leasePid,
          reportedBy,
          capabilityPid,
          restartRemaining,
          restartCount,
          issuedPids

vars == <<leaseState, leasePid, reportedBy, capabilityPid, restartRemaining,
          restartCount, issuedPids>>

Init ==
    /\ leaseState = [s \in Services |-> Unlaunched]
    /\ leasePid = [s \in Services |-> NoPid]
    /\ reportedBy = [s \in Services |-> NoSupervisor]
    /\ capabilityPid = [s \in Services |-> NoPid]
    /\ restartRemaining = [s \in Services |-> InitialRestartBudget]
    /\ restartCount = [s \in Services |-> 0]
    /\ issuedPids = {}

InitialLaunch(s, p, supervisor) ==
    /\ s \in Services
    /\ p \in Pids \ issuedPids
    /\ supervisor = SupervisorFor(s)
    /\ leaseState[s] = Unlaunched
    /\ leaseState' = [leaseState EXCEPT ![s] = PendingReport]
    /\ leasePid' = [leasePid EXCEPT ![s] = p]
    /\ reportedBy' = [reportedBy EXCEPT ![s] = NoSupervisor]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<capabilityPid, restartRemaining, restartCount>>

ReportReadiness(s, p, supervisor) ==
    /\ s \in Services
    /\ p \in Pids
    /\ supervisor = SupervisorFor(s)
    /\ leaseState[s] = PendingReport
    /\ leasePid[s] = p
    /\ reportedBy[s] = NoSupervisor
    /\ leaseState' = [leaseState EXCEPT ![s] = Running]
    /\ reportedBy' = [reportedBy EXCEPT ![s] = supervisor]
    /\ UNCHANGED <<leasePid, capabilityPid, restartRemaining, restartCount, issuedPids>>

GrantCapability(s, p) ==
    /\ s \in Services
    /\ p \in Pids
    /\ leaseState[s] = Running
    /\ leasePid[s] = p
    /\ reportedBy[s] = SupervisorFor(s)
    /\ capabilityPid[s] = NoPid
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = p]
    /\ UNCHANGED <<leaseState, leasePid, reportedBy, restartRemaining,
                  restartCount, issuedPids>>

Exit(s) ==
    /\ s \in Services
    /\ leaseState[s] \in {PendingReport, Running}
    /\ leaseState' = [leaseState EXCEPT ![s] = Exited]
    /\ leasePid' = [leasePid EXCEPT ![s] = NoPid]
    /\ reportedBy' = [reportedBy EXCEPT ![s] = NoSupervisor]
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = NoPid]
    /\ UNCHANGED <<restartRemaining, restartCount, issuedPids>>

Restart(s, p, supervisor) ==
    /\ s \in Services
    /\ p \in Pids \ issuedPids
    /\ supervisor = SupervisorFor(s)
    /\ leaseState[s] = Exited
    /\ restartRemaining[s] > 0
    /\ leaseState' = [leaseState EXCEPT ![s] = PendingReport]
    /\ leasePid' = [leasePid EXCEPT ![s] = p]
    /\ reportedBy' = [reportedBy EXCEPT ![s] = NoSupervisor]
    /\ restartRemaining' = [restartRemaining EXCEPT ![s] = @ - 1]
    /\ restartCount' = [restartCount EXCEPT ![s] = @ + 1]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<capabilityPid>>

Exhaust(s) ==
    /\ s \in Services
    /\ leaseState[s] = Exited
    /\ restartRemaining[s] = 0
    /\ leaseState' = [leaseState EXCEPT ![s] = Failed]
    /\ UNCHANGED <<leasePid, reportedBy, capabilityPid, restartRemaining,
                  restartCount, issuedPids>>

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

TypeOK ==
    /\ Services = {"netd", "uiserver"}
    /\ Supervisors = {InitdSupervisor, RuntimedSupervisor}
    /\ Pids \subseteq Nat
    /\ NoPid \notin Pids
    /\ InitialRestartBudget \in Nat
    /\ leaseState \in [Services -> {Unlaunched, PendingReport, Running, Exited, Failed}]
    /\ leasePid \in [Services -> (Pids \cup {NoPid})]
    /\ reportedBy \in [Services -> (Supervisors \cup {NoSupervisor})]
    /\ capabilityPid \in [Services -> (Pids \cup {NoPid})]
    /\ restartRemaining \in [Services -> 0..InitialRestartBudget]
    /\ restartCount \in [Services -> 0..InitialRestartBudget]
    /\ issuedPids \subseteq Pids

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
            /\ capabilityPid[s] \in {NoPid, leasePid[s]}

CapabilityNeedsExactRunningReportedPid ==
    \A s \in Services:
        capabilityPid[s] # NoPid =>
            /\ leaseState[s] = Running
            /\ capabilityPid[s] = leasePid[s]
            /\ reportedBy[s] = SupervisorFor(s)

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

=============================================================================
