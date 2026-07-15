----------------------- MODULE InputIngestionWorker ------------------------
EXTENDS Naturals

(******************************************************************************
The input-ring consumer must not depend on an application's poll/read cadence.

Concrete source anchors:
  * `kernel/compat/.../input_broker_ops.rs`:
    `SYS_RUSTOS_INPUT_WAIT_BROKER` arms a task before sleeping and rechecks
    producer/consumer after registration.
  * `services/inputd/src/main.rs`: `inputd-dvm-ingress` is the sole
    event-driven worker that invokes the existing bounded ingest broker.
  * `kernel/io-manager/src/input/{dvm_ring,event_queue}.rs`: the MSI-X leaf
    wakes the dedicated inputd slot independently of application poll waiters;
    it neither decodes nor advances the consumer.

The model deliberately includes client polls as unrelated actions. They may
establish policy readiness, but after that point they cannot be required for
consumer progress, and they can never advance the fixed-ring cursor.
*******************************************************************************)

CONSTANTS Slots, Batch, MaxProduced, MaxClientPolls

NoOwner == "none"
WorkerOwner == "inputd-ingestion-worker"
Waiting == "waiting"
Draining == "draining"

VARIABLES policyConsumerReady, workerState, producer, consumer, irqPending,
          batchRemaining, clientPolls, inputdWakeSlot, lastConsumerOwner

vars == <<policyConsumerReady, workerState, producer, consumer, irqPending,
          batchRemaining, clientPolls, inputdWakeSlot, lastConsumerOwner>>

Outstanding == producer - consumer
NormalCapacity == Slots - 1

Init ==
    /\ policyConsumerReady = FALSE
    /\ workerState = Waiting
    /\ producer = 0
    /\ consumer = 0
    /\ irqPending = FALSE
    /\ batchRemaining = 0
    /\ clientPolls = 0
    /\ inputdWakeSlot = WorkerOwner
    /\ lastConsumerOwner = NoOwner

\* A real client completes the policy-backed readiness check. The worker was
\* already created by inputd; no client is permitted to become the consumer.
SetPolicyConsumerReady ==
    /\ ~policyConsumerReady
    /\ workerState = Waiting
    /\ policyConsumerReady' = TRUE
    /\ UNCHANGED <<workerState, producer, consumer, irqPending,
                  batchRemaining, clientPolls, inputdWakeSlot, lastConsumerOwner>>

\* L0 can produce only into a policy-ready ring with a live worker and keeps
\* one slot reserved for authenticated cleanup.
Produce ==
    /\ policyConsumerReady
    /\ workerState \in {Waiting, Draining}
    /\ producer < MaxProduced
    /\ Outstanding < NormalCapacity
    /\ producer' = producer + 1
    /\ irqPending' = IF Outstanding = 0 THEN TRUE ELSE irqPending
    /\ UNCHANGED <<policyConsumerReady, workerState, consumer,
                  batchRemaining, clientPolls, inputdWakeSlot, lastConsumerOwner>>

\* The IRQ can be delayed or lost. The wait broker rechecks the raw cursors,
\* so either an edge or outstanding work makes the worker runnable.
WakeIngestionWorker ==
    /\ workerState = Waiting
    /\ inputdWakeSlot = WorkerOwner
    /\ consumer < producer
    /\ workerState' = Draining
    /\ batchRemaining' = Batch
    /\ irqPending' = FALSE
    /\ inputdWakeSlot' = NoOwner
    /\ UNCHANGED <<policyConsumerReady, producer, consumer,
                  clientPolls, lastConsumerOwner>>

\* Only the inputd worker advances RustOS's consumer cursor. Each broker
\* invocation is bounded, even while catching up after a client stall.
DrainOne ==
    /\ workerState = Draining
    /\ batchRemaining > 0
    /\ consumer < producer
    /\ consumer' = consumer + 1
    /\ batchRemaining' = batchRemaining - 1
    /\ lastConsumerOwner' = WorkerOwner
    /\ UNCHANGED <<policyConsumerReady, workerState, producer, irqPending,
                  clientPolls, inputdWakeSlot>>

FinishBoundedBatch ==
    /\ workerState = Draining
    /\ batchRemaining = 0 \/ consumer = producer
    /\ workerState' = Waiting
    /\ batchRemaining' = 0
    /\ inputdWakeSlot' = WorkerOwner
    /\ UNCHANGED <<policyConsumerReady, producer, consumer, irqPending,
                  clientPolls, lastConsumerOwner>>

\* An application may poll arbitrarily often or stop polling altogether. It
\* neither wakes the worker nor changes producer/consumer ownership.
ClientPoll ==
    /\ clientPolls < MaxClientPolls
    /\ clientPolls' = clientPolls + 1
    /\ UNCHANGED <<policyConsumerReady, workerState, producer, consumer,
                  irqPending, batchRemaining, inputdWakeSlot, lastConsumerOwner>>

Next ==
    \/ SetPolicyConsumerReady
    \/ Produce
    \/ WakeIngestionWorker
    \/ DrainOne
    \/ FinishBoundedBatch
    \/ ClientPoll

TypeOK ==
    /\ policyConsumerReady \in BOOLEAN
    /\ workerState \in {Waiting, Draining}
    /\ producer \in 0..MaxProduced
    /\ consumer \in 0..MaxProduced
    /\ irqPending \in BOOLEAN
    /\ batchRemaining \in 0..Batch
    /\ clientPolls \in 0..MaxClientPolls
    /\ inputdWakeSlot \in {NoOwner, WorkerOwner}
    /\ lastConsumerOwner \in {NoOwner, WorkerOwner}

CursorBound ==
    /\ producer >= consumer
    /\ Outstanding <= Slots

AdmissionHasIndependentConsumer ==
    policyConsumerReady => workerState \in {Waiting, Draining}

OnlyWorkerAdvancesConsumer ==
    lastConsumerOwner \in {NoOwner, WorkerOwner}

BoundedWorkerTurn ==
    /\ batchRemaining <= Batch
    /\ workerState = Waiting => batchRemaining = 0

DedicatedWakeSlot ==
    /\ workerState = Waiting => inputdWakeSlot = WorkerOwner
    /\ workerState = Draining => inputdWakeSlot = NoOwner

ClientPollCannotOwnProgress ==
    /\ clientPolls \in 0..MaxClientPolls
    /\ lastConsumerOwner # "client"

Spec ==
    Init /\ [][Next]_vars
    /\ WF_vars(WakeIngestionWorker)
    /\ WF_vars(DrainOne)
    /\ WF_vars(FinishBoundedBatch)

\* Since L0 has a finite admitted production budget in this model, every
\* outstanding record drains without relying on a later app poll.
RingEventuallyDrains ==
    []((consumer < producer) => <>(consumer = producer))
=============================================================================
