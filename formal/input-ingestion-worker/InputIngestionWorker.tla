----------------------- MODULE InputIngestionWorker ------------------------
EXTENDS Naturals

(******************************************************************************
The input-ring consumer must not depend on an application's poll/read cadence.

Concrete source anchors:
  * `kernel/compat/.../input_broker_ops.rs`:
    `SYS_RUSTOS_INPUT_WAIT_BROKER` arms a task before sleeping and rechecks
    producer/consumer after registration.
  * `services/inputd/src/main.rs`: `inputd-dvm-ingress` is the sole
    event-driven worker that invokes the existing bounded ingest broker. A
    full batch yields and immediately starts another bounded broker turn; it
    does not wait for a later producer record to finish admitted backlog.
  * `kernel/io-manager/src/input/{dvm_ring,wait_queue}.rs`: inputd publishes a
    monotonic consumer wake generation after registering its dedicated slot;
    L0 samples it after commit and the MSI-X leaf only wakes that slot.
    A bounded timer independently wakes the same armed task for an
    authoritative cursor recheck if an interrupt edge is lost or coalesced.

The model deliberately includes client polls as unrelated actions. They may
establish policy readiness, but after that point they cannot be required for
consumer progress, and they can never advance the fixed-ring cursor.
*******************************************************************************)

CONSTANTS Slots, Batch, MaxProduced, MaxClientPolls, MaxWakeGeneration

NoOwner == "none"
WorkerOwner == "inputd-ingestion-worker"
Waiting == "waiting"
Draining == "draining"
Syncing == "syncing-authority"
Exited == "exited"

VARIABLES policyConsumerReady, workerState, producer, consumer, irqPending,
          consumerWakeGeneration, notifiedWakeGeneration,
          batchRemaining, clientPolls, inputdWakeSlot, lastConsumerOwner

vars == <<policyConsumerReady, workerState, producer, consumer, irqPending,
          consumerWakeGeneration, notifiedWakeGeneration,
          batchRemaining, clientPolls, inputdWakeSlot, lastConsumerOwner>>

Outstanding == producer - consumer
NormalCapacity == Slots - 1

Init ==
    /\ policyConsumerReady = FALSE
    /\ workerState = Waiting
    /\ producer = 0
    /\ consumer = 0
    /\ irqPending = FALSE
    /\ consumerWakeGeneration = 0
    /\ notifiedWakeGeneration = 0
    /\ batchRemaining = 0
    /\ clientPolls = 0
    /\ inputdWakeSlot = NoOwner
    /\ lastConsumerOwner = NoOwner

\* A real client completes the policy-backed readiness check. The worker was
\* already created by inputd; no client is permitted to become the consumer.
SetPolicyConsumerReady ==
    /\ ~policyConsumerReady
    /\ workerState = Waiting
    /\ policyConsumerReady' = TRUE
    /\ UNCHANGED <<workerState, producer, consumer, irqPending,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  batchRemaining, clientPolls, inputdWakeSlot, lastConsumerOwner>>

\* inputd registers its task first, publishes a new single-writer generation,
\* then rechecks producer/consumer before committing the block.
ArmIngestionWorker ==
    /\ policyConsumerReady
    /\ workerState = Waiting
    /\ inputdWakeSlot = NoOwner
    /\ consumer = producer
    /\ consumerWakeGeneration < MaxWakeGeneration
    /\ inputdWakeSlot' = WorkerOwner
    /\ consumerWakeGeneration' = consumerWakeGeneration + 1
    /\ UNCHANGED <<policyConsumerReady, workerState, producer, consumer,
                  irqPending, notifiedWakeGeneration, batchRemaining,
                  clientPolls, lastConsumerOwner>>

\* L0 can produce only into a policy-ready ring with a live worker and keeps
\* one slot reserved for authenticated cleanup.
Produce ==
    /\ policyConsumerReady
    /\ workerState \in {Waiting, Draining, Syncing}
    /\ producer < MaxProduced
    /\ Outstanding < NormalCapacity
    /\ producer' = producer + 1
    \* L0 samples the arm generation after commit and emits at most one edge
    \* for that generation. Records committed while the worker remains
    \* runnable are therefore batched without weakening lost-wake safety.
    /\ irqPending' =
        IF consumerWakeGeneration # notifiedWakeGeneration
        THEN TRUE
        ELSE irqPending
    /\ notifiedWakeGeneration' = consumerWakeGeneration
    /\ UNCHANGED <<policyConsumerReady, workerState, consumer,
                  consumerWakeGeneration, batchRemaining, clientPolls,
                  inputdWakeSlot, lastConsumerOwner>>

\* The IRQ can be delayed or lost. The wait broker rechecks the raw cursors,
\* so either an edge or outstanding work makes the worker runnable.
WakeIngestionWorker ==
    /\ workerState = Waiting
    /\ inputdWakeSlot = WorkerOwner
    /\ consumer < producer
    /\ irqPending
    /\ workerState' = Draining
    /\ batchRemaining' = Batch
    /\ irqPending' = FALSE
    /\ inputdWakeSlot' = NoOwner
    /\ UNCHANGED <<policyConsumerReady, producer, consumer,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  clientPolls, lastConsumerOwner>>

\* The finite kernel timer is independent of MSI-X delivery. It turns a lost
\* or indefinitely coalesced interrupt into the same authoritative cursor
\* recheck. Empty watchdog expiries are abstract stuttering steps.
WatchdogWakeIngestionWorker ==
    /\ workerState = Waiting
    /\ inputdWakeSlot = WorkerOwner
    /\ consumer < producer
    /\ workerState' = Draining
    /\ batchRemaining' = Batch
    /\ inputdWakeSlot' = NoOwner
    /\ UNCHANGED <<policyConsumerReady, producer, consumer, irqPending,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  clientPolls, lastConsumerOwner>>

\* A commit that raced before arm publication is found by the authoritative
\* post-registration cursor recheck; it needs no fabricated interrupt.
ObserveBacklogWithoutSleep ==
    /\ workerState = Waiting
    /\ inputdWakeSlot = NoOwner
    /\ consumer < producer
    /\ workerState' = Draining
    /\ batchRemaining' = Batch
    /\ UNCHANGED <<policyConsumerReady, producer, consumer, irqPending,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  clientPolls, inputdWakeSlot, lastConsumerOwner>>

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
                  consumerWakeGeneration, notifiedWakeGeneration,
                  clientPolls, inputdWakeSlot>>

\* Session start/end synchronizes the authenticated DVM epoch with netd.
\* `Syncing` denotes that inputd has released its policy queue lock before the
\* bounded cross-service IPC. A sync failure resets the uncommitted decoder
\* session and resumes draining; it cannot kill the sole ring consumer.
BeginAuthoritySync ==
    /\ workerState = Draining
    /\ workerState' = Syncing
    /\ UNCHANGED <<policyConsumerReady, producer, consumer, irqPending,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  batchRemaining, clientPolls, inputdWakeSlot,
                  lastConsumerOwner>>

\* Success and failure both settle the in-flight IPC before the worker resumes.
\* The source witnesses distinguish decoder reset; the composition needs only
\* the common lifecycle edge because neither outcome owns ring progress.
SettleAuthoritySync ==
    /\ workerState = Syncing
    /\ workerState' = Draining
    /\ UNCHANGED <<policyConsumerReady, producer, consumer, irqPending,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  batchRemaining, clientPolls, inputdWakeSlot,
                  lastConsumerOwner>>

\* Process-exit cleanup revokes the service endpoint and the independent fixed
\* ring producer-admission lease as one lifecycle transition. Records admitted
\* for the dead owner are retired before a replacement worker may rearm.
ConsumerOwnerExit ==
    /\ workerState \in {Waiting, Draining, Syncing}
    /\ policyConsumerReady' = FALSE
    /\ workerState' = Exited
    /\ consumer' = producer
    /\ irqPending' = FALSE
    /\ inputdWakeSlot' = NoOwner
    /\ batchRemaining' = 0
    /\ UNCHANGED <<producer, consumerWakeGeneration,
                  notifiedWakeGeneration, clientPolls,
                  lastConsumerOwner>>

RestartWorker ==
    /\ workerState = Exited
    /\ ~policyConsumerReady
    /\ workerState' = Waiting
    /\ UNCHANGED <<policyConsumerReady, producer, consumer, irqPending,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  batchRemaining, clientPolls, inputdWakeSlot,
                  lastConsumerOwner>>

FinishBoundedBatch ==
    /\ workerState = Draining
    /\ batchRemaining = 0 \/ consumer = producer
    /\ workerState' = Waiting
    /\ batchRemaining' = 0
    /\ inputdWakeSlot' = NoOwner
    /\ UNCHANGED <<policyConsumerReady, producer, consumer, irqPending,
                  consumerWakeGeneration, notifiedWakeGeneration,
                  clientPolls, lastConsumerOwner>>

\* An application may poll arbitrarily often or stop polling altogether. It
\* neither wakes the worker nor changes producer/consumer ownership.
ClientPoll ==
    /\ clientPolls < MaxClientPolls
    /\ clientPolls' = clientPolls + 1
    /\ UNCHANGED <<policyConsumerReady, workerState, producer, consumer,
                  irqPending, consumerWakeGeneration, notifiedWakeGeneration,
                  batchRemaining, inputdWakeSlot, lastConsumerOwner>>

Next ==
    \/ SetPolicyConsumerReady
    \/ ArmIngestionWorker
    \/ Produce
    \/ WakeIngestionWorker
    \/ WatchdogWakeIngestionWorker
    \/ ObserveBacklogWithoutSleep
    \/ DrainOne
    \/ BeginAuthoritySync
    \/ SettleAuthoritySync
    \/ ConsumerOwnerExit
    \/ RestartWorker
    \/ FinishBoundedBatch
    \/ ClientPoll

TypeOK ==
    /\ policyConsumerReady \in BOOLEAN
    /\ workerState \in {Waiting, Draining, Syncing, Exited}
    /\ producer \in 0..MaxProduced
    /\ consumer \in 0..MaxProduced
    /\ irqPending \in BOOLEAN
    /\ consumerWakeGeneration \in 0..MaxWakeGeneration
    /\ notifiedWakeGeneration \in 0..MaxWakeGeneration
    /\ notifiedWakeGeneration <= consumerWakeGeneration
    /\ batchRemaining \in 0..Batch
    /\ clientPolls \in 0..MaxClientPolls
    /\ inputdWakeSlot \in {NoOwner, WorkerOwner}
    /\ lastConsumerOwner \in {NoOwner, WorkerOwner}

CursorBound ==
    /\ producer >= consumer
    /\ Outstanding <= Slots

AdmissionHasIndependentConsumer ==
    policyConsumerReady => workerState \in {Waiting, Draining, Syncing}

OnlyWorkerAdvancesConsumer ==
    lastConsumerOwner \in {NoOwner, WorkerOwner}

BoundedWorkerTurn ==
    /\ batchRemaining <= Batch
    /\ workerState = Waiting => batchRemaining = 0

DedicatedWakeSlot ==
    /\ workerState = Waiting => inputdWakeSlot \in {NoOwner, WorkerOwner}
    /\ workerState \in {Draining, Syncing, Exited} => inputdWakeSlot = NoOwner

ExitedWorkerCannotRetainProducerAdmission ==
    workerState = Exited => ~policyConsumerReady

ClientPollCannotOwnProgress ==
    /\ clientPolls \in 0..MaxClientPolls
    /\ lastConsumerOwner # "client"

Spec ==
    Init /\ [][Next]_vars
    /\ WF_vars(ArmIngestionWorker)
    /\ WF_vars(WakeIngestionWorker)
    /\ WF_vars(WatchdogWakeIngestionWorker)
    /\ WF_vars(ObserveBacklogWithoutSleep)
    /\ WF_vars(DrainOne)
    /\ WF_vars(SettleAuthoritySync)
    /\ WF_vars(ConsumerOwnerExit)
    /\ WF_vars(RestartWorker)
    /\ WF_vars(FinishBoundedBatch)

\* Since L0 has a finite admitted production budget in this model, every
\* outstanding record drains or loses producer admission through the exact
\* policy-owner failure terminal, without relying on a later app poll.
RingEventuallySettles ==
    []((consumer < producer) => <>(consumer = producer \/ ~policyConsumerReady))
=============================================================================
