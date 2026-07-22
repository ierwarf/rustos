--------------------- MODULE ProcessSignalDelivery ---------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: procd disposition policy plus ring0 pending-bit/frame substrate.
Linearization point: ring0 revalidation and pending-bit removal immediately
before ignore, terminate, or handler-frame installation. Policy replies may
become stale while a signal is masked or consumed and must then be rejected.
***************************************************************************)

CONSTANTS Signals, Unblockable, KillSignals, StopSignals, FaultSignal
NoSignal == 0
NoneAction == "none"
Ignore == "ignore"
Terminate == "terminate"
Handler == "handler"
Stop == "stop"
NoOutcome == "no-outcome"
Delivered == "delivered"
Rejected == "rejected"
Running == "running"
Stopped == "stopped"
Terminated == "terminated"
ProcessStates == {Running, Stopped, Terminated}
NoFault == "no-fault"
FaultRecovered == "recovered"
FaultTerminated == "terminated"
VARIABLES pending, masked, selected, action, targetValid, processState, consumed,
          terminationEvidence, runtimeAuthority, taskIpcAuthority,
          processIpcAuthority, lastFaultOutcome,
          lastOutcome, lastSignal, lastAction
vars == <<pending, masked, selected, action, targetValid, processState, consumed,
          terminationEvidence, runtimeAuthority, taskIpcAuthority,
          processIpcAuthority, lastFaultOutcome,
          lastOutcome, lastSignal, lastAction>>

Init ==
    /\ pending = {} /\ masked = {} /\ selected = NoSignal
    /\ action = NoneAction /\ targetValid = FALSE /\ processState = Running
    /\ consumed = {} /\ terminationEvidence = FALSE /\ runtimeAuthority = TRUE
    /\ taskIpcAuthority = TRUE
    /\ processIpcAuthority = TRUE
    /\ lastFaultOutcome = NoFault /\ lastOutcome = NoOutcome
    /\ lastSignal = NoSignal /\ lastAction = NoneAction

Queue(s) ==
    /\ processState # Terminated /\ s \in Signals
    /\ pending' = pending \cup {s}
    /\ UNCHANGED <<masked, selected, action, targetValid, processState, consumed,
                    terminationEvidence, runtimeAuthority, taskIpcAuthority,
                    processIpcAuthority, lastFaultOutcome,
                    lastOutcome, lastSignal, lastAction>>

SetMask(s, value) ==
    /\ processState # Terminated /\ s \in Signals /\ value \in BOOLEAN
    /\ (~value \/ s \notin Unblockable)
    /\ masked' = IF value THEN masked \cup {s} ELSE masked \ {s}
    /\ UNCHANGED <<pending, selected, action, targetValid, processState, consumed,
                    terminationEvidence, runtimeAuthority, taskIpcAuthority,
                    processIpcAuthority, lastFaultOutcome,
                    lastOutcome, lastSignal, lastAction>>

PolicySelect(s, a, valid) ==
    /\ processState = Running /\ selected = NoSignal /\ s \in Signals
    /\ a \in {Ignore, Terminate, Handler, Stop} /\ valid \in BOOLEAN
    /\ selected' = s /\ action' = a /\ targetValid' = valid
    /\ lastFaultOutcome' = NoFault
    /\ UNCHANGED <<pending, masked, processState, consumed, terminationEvidence,
                    runtimeAuthority, taskIpcAuthority, processIpcAuthority,
                    lastOutcome, lastSignal,
                    lastAction>>

Commit ==
    /\ selected # NoSignal /\ selected \in pending \ masked
    /\ (action # Handler \/ targetValid)
    /\ (action # Stop \/ selected \in StopSignals)
    /\ (selected \notin KillSignals \/ action = Terminate)
    /\ (selected \notin StopSignals \/ action = Stop)
    /\ pending' = IF action = Terminate THEN {} ELSE pending \ {selected}
    /\ consumed' = consumed \cup {selected}
    /\ processState' = IF action = Terminate THEN Terminated
                       ELSE IF action = Stop THEN Stopped ELSE processState
    /\ terminationEvidence' = (terminationEvidence \/ (action = Terminate))
    /\ runtimeAuthority' = (runtimeAuthority /\ (action # Terminate))
    /\ taskIpcAuthority' = (taskIpcAuthority /\ (action # Terminate))
    /\ processIpcAuthority' = (processIpcAuthority /\ (action # Terminate))
    /\ lastFaultOutcome' = NoFault
    /\ selected' = NoSignal /\ action' = NoneAction /\ targetValid' = FALSE
    /\ lastOutcome' = Delivered /\ lastSignal' = selected /\ lastAction' = action
    /\ UNCHANGED masked

RejectStaleOrInvalid ==
    /\ selected # NoSignal
    /\ (selected \notin pending \ masked \/ (action = Handler /\ ~targetValid)
        \/ (selected \in KillSignals /\ action # Terminate)
        \/ (selected \in StopSignals /\ action # Stop))
    /\ selected' = NoSignal /\ action' = NoneAction /\ targetValid' = FALSE
    /\ lastOutcome' = Rejected /\ lastSignal' = selected /\ lastAction' = action
    /\ lastFaultOutcome' = NoFault
    /\ UNCHANGED <<pending, masked, processState, consumed, terminationEvidence,
                    runtimeAuthority, taskIpcAuthority, processIpcAuthority>>

(***************************************************************************
The HAL may first classify a page fault as recoverable stack growth.  That
decision precedes all process cleanup: a resumed thread retains its endpoint
and process-policy authority.  Only actual retirement of the final thread may
commit the same lifecycle evidence used by explicit and signal exits.
***************************************************************************)
RecoverableFault ==
    /\ processState = Running
    /\ lastFaultOutcome' = FaultRecovered
    /\ UNCHANGED <<pending, masked, selected, action, targetValid, processState,
                    consumed, terminationEvidence, runtimeAuthority, taskIpcAuthority,
                    processIpcAuthority,
                    lastOutcome, lastSignal, lastAction>>

FatalFault ==
    /\ processState = Running
    /\ pending' = {}
    /\ selected' = NoSignal /\ action' = NoneAction /\ targetValid' = FALSE
    /\ processState' = Terminated
    /\ terminationEvidence' = TRUE
    /\ runtimeAuthority' = FALSE
    /\ taskIpcAuthority' = FALSE
    /\ processIpcAuthority' = FALSE
    /\ lastFaultOutcome' = FaultTerminated
    /\ lastOutcome' = Delivered /\ lastSignal' = FaultSignal
    /\ lastAction' = Terminate
    /\ UNCHANGED <<masked, consumed>>

Next ==
    \/ \E s \in Signals: Queue(s)
    \/ \E s \in Signals, value \in BOOLEAN: SetMask(s, value)
    \/ \E s \in Signals, a \in {Ignore, Terminate, Handler, Stop}, valid \in BOOLEAN:
           PolicySelect(s, a, valid)
    \/ Commit \/ RejectStaleOrInvalid
    \/ RecoverableFault \/ FatalFault
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ pending \in SUBSET Signals /\ masked \in SUBSET Signals
    /\ Unblockable \subseteq Signals
    /\ KillSignals \subseteq Unblockable
    /\ StopSignals \subseteq Unblockable
    /\ KillSignals \cap StopSignals = {}
    /\ Unblockable = KillSignals \cup StopSignals
    /\ FaultSignal \in Signals
    /\ selected \in Signals \cup {NoSignal}
    /\ action \in {NoneAction, Ignore, Terminate, Handler, Stop}
    /\ targetValid \in BOOLEAN /\ processState \in ProcessStates
    /\ consumed \in SUBSET Signals
    /\ terminationEvidence \in BOOLEAN
    /\ runtimeAuthority \in BOOLEAN
    /\ taskIpcAuthority \in BOOLEAN
    /\ processIpcAuthority \in BOOLEAN
    /\ lastFaultOutcome \in {NoFault, FaultRecovered, FaultTerminated}
    /\ lastOutcome \in {NoOutcome, Delivered, Rejected}
    /\ lastSignal \in Signals \cup {NoSignal}
    /\ lastAction \in {NoneAction, Ignore, Terminate, Handler, Stop}
NoSignalHasNoPolicyAuthority == selected = NoSignal => action = NoneAction /\ ~targetValid
UnblockableSignalsAreNeverMasked == masked \cap Unblockable = {}
DeliveredKillOnlyTerminates ==
    lastOutcome = Delivered /\ lastSignal \in KillSignals => lastAction = Terminate
DeliveredStopOnlyStops ==
    lastOutcome = Delivered /\ lastSignal \in StopSignals => lastAction = Stop
TerminatedProcessRetainsNoPendingAuthority == processState = Terminated => pending = {}
TerminatedProcessHasLifecycleEvidence == processState = Terminated => terminationEvidence
TerminatedProcessRetainsNoRuntimeAuthority == processState = Terminated => ~runtimeAuthority
TerminatedTaskRetainsNoIpcAuthority == processState = Terminated => ~taskIpcAuthority
TerminatedProcessRetainsNoIpcAuthority == processState = Terminated => ~processIpcAuthority
RecoverableFaultPreservesRuntimeAuthority ==
    lastFaultOutcome = FaultRecovered =>
        /\ processState = Running
        /\ runtimeAuthority
        /\ taskIpcAuthority
        /\ processIpcAuthority
FatalFaultCommitsLifecycleEvidence ==
    lastFaultOutcome = FaultTerminated =>
        /\ processState = Terminated
        /\ terminationEvidence
        /\ ~runtimeAuthority
        /\ ~taskIpcAuthority
        /\ ~processIpcAuthority
StoppedProcessRecordsStopDisposition ==
    processState = Stopped =>
        /\ lastOutcome = Delivered
        /\ lastSignal \in StopSignals
        /\ lastAction = Stop

=============================================================================
