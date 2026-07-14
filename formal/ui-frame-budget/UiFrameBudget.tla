------------------------------ MODULE UiFrameBudget -----------------------------
EXTENDS Naturals, Sequences, FiniteSets

(*******************************************************************************
Models the uiserver keyboard-to-frame boundary after inputd has delivered an
already validated event.

Concrete owners:
  * services/uiserver/src/input_loop.rs `process_pending_input`
  * services/uiserver/src/app/input.rs `handle_input_event`
  * services/uiserver/src/app/runtime.rs `ConsoleCommandDispatcher`
  * services/uiserver/src/main.rs profile and frame/present loop

Keyboard routing and console focus updates cross `uiserver -> devmgrd ->
`runtimed`. That policy IPC can wait while a service is recovering. The UI
loop must instead make one bounded admission decision: enqueue the command for
the delivery worker or record a visible rejection. It dirties the local UI
state in the same atomic step and never waits for the external reply. The
worker preserves FIFO order and may remain in flight indefinitely; rendering
remains independently enabled while it does.

Linearization points:
  * UiDispatch changes an event from UI ownership to the bounded worker queue
    (or to a explicitly recorded rejection) and records its redraw debt.
  * StartDelivery removes only the queue head and makes it the sole in-flight
    console request.
  * CompleteDelivery / FailDelivery terminally consume that exact request.
  * Present consumes one redraw debt without consulting delivery completion.

This abstraction does not claim that a terminal receives every key after its
policy endpoint fails.  It proves the fail-closed alternative: overload is
bounded and observable, and a stalled endpoint cannot own the UI frame loop.
*******************************************************************************)

CONSTANTS EventCount, QueueCapacity, MaxTick

Events == 1..EventCount
NoEvent == 0

Source == "source"
Ui == "ui"
Queue == "queue"
Worker == "worker"
Done == "done"
Rejected == "rejected"

NoResult == "none"
Delivered == "delivered"
DeliveryError == "delivery-error"

UiIdle == "idle"
UiInput == "input"
UiRender == "render"

VARIABLES now,
          nextSource,
          owner,
          deliveryQueue,
          deliveryInFlight,
          deliveryResult,
          dirty,
          presented,
          rejectionLogged,
          uiPhase,
          uiSteps

vars == <<now, nextSource, owner, deliveryQueue, deliveryInFlight,
          deliveryResult, dirty, presented, rejectionLogged, uiPhase, uiSteps>>

SeqSet(sequence) == {sequence[index] : index \in 1..Len(sequence)}

Init ==
    /\ now = 0
    /\ nextSource = 1
    /\ owner = [event \in Events |-> Source]
    /\ deliveryQueue = <<>>
    /\ deliveryInFlight = NoEvent
    /\ deliveryResult = [event \in Events |-> NoResult]
    /\ dirty = [event \in Events |-> FALSE]
    /\ presented = [event \in Events |-> FALSE]
    /\ rejectionLogged = [event \in Events |-> FALSE]
    /\ uiPhase = UiIdle
    /\ uiSteps = 0

(*******************************************************************************
Inputd delivers events in source order.  The model permits the source to get
ahead of uiserver so TLC also explores a non-empty UI ingress backlog.
*******************************************************************************)
PublishInput ==
    /\ nextSource \in Events
    /\ owner[nextSource] = Source
    /\ owner' = [owner EXCEPT ![nextSource] = Ui]
    /\ nextSource' = nextSource + 1
    /\ UNCHANGED <<now, deliveryQueue, deliveryInFlight, deliveryResult,
                  dirty, presented, rejectionLogged, uiPhase, uiSteps>>

(*******************************************************************************
The UI processes its oldest ingress event.  Admission is non-blocking: full
capacity produces a recorded rejection rather than a wait.  Both outcomes
leave a local redraw debt, so input feedback is not coupled to policy IPC.
*******************************************************************************)
UiDispatch ==
    \E event \in Events:
        /\ owner[event] = Ui
        /\ \A earlier \in 1..(event - 1): owner[earlier] # Ui
        /\ dirty' = [dirty EXCEPT ![event] = TRUE]
        /\ uiPhase' = UiInput
        /\ uiSteps' = uiSteps + 1
        /\ IF Len(deliveryQueue) < QueueCapacity
              THEN /\ owner' = [owner EXCEPT ![event] = Queue]
                   /\ deliveryQueue' = Append(deliveryQueue, event)
                   /\ rejectionLogged' = rejectionLogged
              ELSE /\ owner' = [owner EXCEPT ![event] = Rejected]
                   /\ deliveryQueue' = deliveryQueue
                   /\ rejectionLogged' = [rejectionLogged EXCEPT ![event] = TRUE]
        /\ UNCHANGED <<now, nextSource, deliveryInFlight, deliveryResult,
                      presented>>

(*******************************************************************************
Only the worker performs the synchronous console operation.  It has exactly
one in-flight request and starts from the FIFO queue head.
*******************************************************************************)
StartDelivery ==
    /\ deliveryInFlight = NoEvent
    /\ Len(deliveryQueue) > 0
    /\ LET event == Head(deliveryQueue) IN
       /\ owner[event] = Queue
       /\ owner' = [owner EXCEPT ![event] = Worker]
       /\ deliveryQueue' = Tail(deliveryQueue)
       /\ deliveryInFlight' = event
    /\ UNCHANGED <<now, nextSource, deliveryResult, dirty, presented,
                  rejectionLogged, uiPhase, uiSteps>>

CompleteDelivery ==
    /\ deliveryInFlight \in Events
    /\ owner[deliveryInFlight] = Worker
    /\ owner' = [owner EXCEPT ![deliveryInFlight] = Done]
    /\ deliveryResult' = [deliveryResult EXCEPT ![deliveryInFlight] = Delivered]
    /\ deliveryInFlight' = NoEvent
    /\ UNCHANGED <<now, nextSource, deliveryQueue, dirty, presented,
                  rejectionLogged, uiPhase, uiSteps>>

FailDelivery ==
    /\ deliveryInFlight \in Events
    /\ owner[deliveryInFlight] = Worker
    /\ owner' = [owner EXCEPT ![deliveryInFlight] = Done]
    /\ deliveryResult' = [deliveryResult EXCEPT ![deliveryInFlight] = DeliveryError]
    /\ deliveryInFlight' = NoEvent
    /\ UNCHANGED <<now, nextSource, deliveryQueue, dirty, presented,
                  rejectionLogged, uiPhase, uiSteps>>

(*******************************************************************************
An endpoint may remain blocked.  Advancing this worker-only clock deliberately
does not change UI ownership, phase, or redraw debt; Present remains enabled.
*******************************************************************************)
WorkerStalls ==
    /\ deliveryInFlight \in Events
    /\ now < MaxTick
    /\ now' = now + 1
    /\ UNCHANGED <<nextSource, owner, deliveryQueue, deliveryInFlight,
                  deliveryResult, dirty, presented, rejectionLogged, uiPhase,
                  uiSteps>>

Present ==
    \E event \in Events:
        /\ dirty[event]
        /\ dirty' = [dirty EXCEPT ![event] = FALSE]
        /\ presented' = [presented EXCEPT ![event] = TRUE]
        /\ uiPhase' = UiRender
        /\ uiSteps' = uiSteps + 1
        /\ UNCHANGED <<now, nextSource, owner, deliveryQueue,
                      deliveryInFlight, deliveryResult, rejectionLogged>>

ReturnUiIdle ==
    /\ uiPhase # UiIdle
    /\ uiPhase' = UiIdle
    /\ UNCHANGED <<now, nextSource, owner, deliveryQueue, deliveryInFlight,
                  deliveryResult, dirty, presented, rejectionLogged, uiSteps>>

Next ==
    \/ PublishInput
    \/ UiDispatch
    \/ StartDelivery
    \/ CompleteDelivery
    \/ FailDelivery
    \/ WorkerStalls
    \/ Present
    \/ ReturnUiIdle

Spec == Init /\ [][Next]_vars /\ WF_vars(UiDispatch) /\ WF_vars(Present)

TypeOK ==
    /\ now \in 0..MaxTick
    /\ nextSource \in 1..(EventCount + 1)
    /\ owner \in [Events -> {Source, Ui, Queue, Worker, Done, Rejected}]
    /\ deliveryQueue \in Seq(Events)
    /\ deliveryInFlight \in Events \cup {NoEvent}
    /\ deliveryResult \in [Events -> {NoResult, Delivered, DeliveryError}]
    /\ dirty \in [Events -> BOOLEAN]
    /\ presented \in [Events -> BOOLEAN]
    /\ rejectionLogged \in [Events -> BOOLEAN]
    /\ uiPhase \in {UiIdle, UiInput, UiRender}
    /\ uiSteps \in Nat

BoundedQueue == Len(deliveryQueue) <= QueueCapacity

QueueOwnership ==
    /\ SeqSet(deliveryQueue) = {event \in Events : owner[event] = Queue}
    /\ Cardinality(SeqSet(deliveryQueue)) = Len(deliveryQueue)

SingleWorkerOwnership ==
    /\ (deliveryInFlight = NoEvent) \/ owner[deliveryInFlight] = Worker
    /\ Cardinality({event \in Events : owner[event] = Worker}) <= 1
    /\ \A event \in Events:
          owner[event] = Worker => event = deliveryInFlight /\ event \notin SeqSet(deliveryQueue)

FifoDelivery ==
    \A first \in 1..Len(deliveryQueue):
        \A second \in 1..Len(deliveryQueue):
            first < second => deliveryQueue[first] < deliveryQueue[second]

TerminalAccounting ==
    \A event \in Events:
        /\ owner[event] = Rejected => rejectionLogged[event]
        /\ deliveryResult[event] # NoResult => owner[event] = Done
        /\ owner[event] = Done => deliveryResult[event] # NoResult

RenderAccounting ==
    \A event \in Events:
        /\ dirty[event] => owner[event] \in {Queue, Worker, Done, Rejected}
        /\ presented[event] => owner[event] \in {Queue, Worker, Done, Rejected}

(*******************************************************************************
No UiConsoleWait state exists.  More importantly, `Present` is weakly fair and
enabled solely by dirty debt, not by `deliveryInFlight` or delivery outcome.
Thus a worker-stall trace cannot suppress local frame progress.
*******************************************************************************)
NoUiConsoleWait == uiPhase \in {UiIdle, UiInput, UiRender}

InputFeedbackEventuallyPresented ==
    \A event \in Events:
        (owner[event] \in {Queue, Worker, Done, Rejected} \/ dirty[event])
            ~> presented[event]

=============================================================================
