------------------------ MODULE ZeroTrustServiceFlow ------------------------
EXTENDS Naturals

(*******************************************************************************
End-to-end contract for one request crossing a published RustOS service
endpoint and, optionally, a second delegated policy service.

Every hop treats bytes and claimed identity as hostile. The kernel stamps the
immediate sender, the receiving owner validates the exact wire shape and either
an exact subject match or a live service-owner delegation, and the object owner
rechecks the capability and generation before mutation. The caller admits only
an exact response bound to the original request.

Concrete owners:
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs
  * libs/rustos-svc-runtime/src/ipc.rs
  * services/* service endpoint receive loops
*******************************************************************************)

Bool == {FALSE, TRUE}
Terminal == {"rejected", "revoked", "timed-out", "admitted"}

VARIABLES phase,
          shapeValid,
          exactSender,
          delegated,
          delegatorLive,
          capabilityValid,
          generationMatch,
          responseBound

vars ==
    <<phase, shapeValid, exactSender, delegated, delegatorLive,
      capabilityValid, generationMatch, responseBound>>

Init ==
    /\ phase = "received"
    /\ shapeValid \in Bool
    /\ exactSender \in Bool
    /\ delegated \in Bool
    /\ delegatorLive \in Bool
    /\ capabilityValid \in Bool
    /\ generationMatch \in Bool
    /\ responseBound \in Bool

RejectIngress ==
    /\ phase = "received"
    /\ (~shapeValid \/ (~exactSender /\ ~(delegated /\ delegatorLive)))
    /\ phase' = "rejected"
    /\ UNCHANGED
        <<shapeValid, exactSender, delegated, delegatorLive,
          capabilityValid, generationMatch, responseBound>>

AdmitIngress ==
    /\ phase = "received"
    /\ shapeValid
    /\ (exactSender \/ (delegated /\ delegatorLive))
    /\ phase' = "owner-check"
    /\ UNCHANGED
        <<shapeValid, exactSender, delegated, delegatorLive,
          capabilityValid, generationMatch, responseBound>>

RejectOwner ==
    /\ phase = "owner-check"
    /\ ~capabilityValid \/ ~generationMatch
    /\ phase' = "revoked"
    /\ UNCHANGED
        <<shapeValid, exactSender, delegated, delegatorLive,
          capabilityValid, generationMatch, responseBound>>

DispatchOwner ==
    /\ phase = "owner-check"
    /\ capabilityValid
    /\ generationMatch
    /\ phase' = "waiting"
    /\ UNCHANGED
        <<shapeValid, exactSender, delegated, delegatorLive,
          capabilityValid, generationMatch, responseBound>>

AdmitResponse ==
    /\ phase = "waiting"
    /\ responseBound
    /\ phase' = "admitted"
    /\ UNCHANGED
        <<shapeValid, exactSender, delegated, delegatorLive,
          capabilityValid, generationMatch, responseBound>>

RejectResponse ==
    /\ phase = "waiting"
    /\ ~responseBound
    /\ phase' = "rejected"
    /\ UNCHANGED
        <<shapeValid, exactSender, delegated, delegatorLive,
          capabilityValid, generationMatch, responseBound>>

Timeout ==
    /\ phase = "waiting"
    /\ phase' = "timed-out"
    /\ UNCHANGED
        <<shapeValid, exactSender, delegated, delegatorLive,
          capabilityValid, generationMatch, responseBound>>

Next ==
    \/ RejectIngress
    \/ AdmitIngress
    \/ RejectOwner
    \/ DispatchOwner
    \/ AdmitResponse
    \/ RejectResponse
    \/ Timeout

TypeOK ==
    /\ phase \in {"received", "owner-check", "waiting"} \cup Terminal
    /\ shapeValid \in Bool
    /\ exactSender \in Bool
    /\ delegated \in Bool
    /\ delegatorLive \in Bool
    /\ capabilityValid \in Bool
    /\ generationMatch \in Bool
    /\ responseBound \in Bool

DispatchRequiresIndependentIngressAdmission ==
    phase \in {"owner-check", "waiting", "admitted"} =>
        /\ shapeValid
        /\ (exactSender \/ (delegated /\ delegatorLive))

DelegationNeverTrustsADeadServiceOwner ==
    phase \in {"owner-check", "waiting", "admitted"} /\ ~exactSender =>
        /\ delegated
        /\ delegatorLive

MutationRequiresObjectAuthority ==
    phase \in {"waiting", "admitted"} =>
        /\ capabilityValid
        /\ generationMatch

SuccessRequiresExactEndToEndBinding ==
    phase = "admitted" =>
        /\ responseBound
        /\ shapeValid
        /\ (exactSender \/ (delegated /\ delegatorLive))
        /\ capabilityValid
        /\ generationMatch

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in Terminal)
=============================================================================
