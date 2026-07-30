-------------------------- MODULE UiMainLoopWakeup --------------------------
EXTENDS Naturals

(***************************************************************************
Models the high-risk uiserver idle-wait boundary. The main loop may sleep
only after observing an empty input stream, and input publication must wake
an arming or sleeping loop atomically. An independent monotonic deadline is
the second wake authority. Neither a lost notification nor a stopped
clockevent may leave admitted input permanently unconsumed.

Concrete owners:
  * services/uiserver/src/input_loop.rs `InputReader::wait_for_wake_until`
  * services/uiserver/src/main.rs idle-deadline selection and presentation
  * kernel/compat futex timeout plus kernel/hal monotonic sleep waiters
***************************************************************************)

CONSTANTS MaxProduced, MaxTick

Active == "active"
Arming == "arming"
Sleeping == "sleeping"

VARIABLES now, producer, consumer, loopState, observedGeneration,
          deadline, dirty, frames

vars == <<now, producer, consumer, loopState, observedGeneration,
          deadline, dirty, frames>>

Init ==
    /\ now = 0
    /\ producer = 0
    /\ consumer = 0
    /\ loopState = Active
    /\ observedGeneration = 0
    /\ deadline = 1
    /\ dirty = FALSE
    /\ frames = 0

\* Publication and wake are one linearization point. A producer cannot leave
\* a newly non-empty stream behind an armed or committed sleep.
PublishInput ==
    /\ producer < MaxProduced
    /\ producer' = producer + 1
    /\ loopState' = Active
    /\ UNCHANGED <<now, consumer, observedGeneration, deadline, dirty, frames>>

ConsumeInput ==
    /\ loopState = Active
    /\ consumer < producer
    /\ consumer' = consumer + 1
    /\ dirty' = TRUE
    /\ UNCHANGED <<now, producer, loopState, observedGeneration, deadline, frames>>

\* Check-arm records the exact provider generation. Commit is legal only if
\* the recheck still observes the same empty generation.
BeginArm ==
    /\ loopState = Active
    /\ consumer = producer
    /\ ~dirty
    /\ now < MaxTick
    /\ loopState' = Arming
    /\ observedGeneration' = producer
    /\ deadline' = now + 1
    /\ UNCHANGED <<now, producer, consumer, dirty, frames>>

CommitSleep ==
    /\ loopState = Arming
    /\ consumer = producer
    /\ observedGeneration = producer
    /\ now < deadline
    /\ loopState' = Sleeping
    /\ UNCHANGED <<now, producer, consumer, observedGeneration,
                  deadline, dirty, frames>>

\* A clockevent catches up from the monotonic clock. Crossing the armed
\* deadline wakes both an arming and a sleeping loop in the same transition.
Tick ==
    /\ now < MaxTick
    /\ now' = now + 1
    /\ loopState' =
        IF loopState \in {Arming, Sleeping} /\ now + 1 >= deadline
        THEN Active
        ELSE loopState
    /\ UNCHANGED <<producer, consumer, observedGeneration,
                  deadline, dirty, frames>>

Present ==
    /\ loopState = Active
    /\ dirty
    /\ dirty' = FALSE
    /\ frames' = frames + 1
    /\ UNCHANGED <<now, producer, consumer, loopState,
                  observedGeneration, deadline>>

TerminalStutter ==
    /\ now = MaxTick
    /\ producer = MaxProduced
    /\ consumer = producer
    /\ ~dirty
    /\ loopState = Active
    /\ UNCHANGED vars

Next ==
    \/ PublishInput
    \/ ConsumeInput
    \/ BeginArm
    \/ CommitSleep
    \/ Tick
    \/ Present
    \/ TerminalStutter

Spec ==
    Init
    /\ [][Next]_vars
    /\ WF_vars(ConsumeInput)
    /\ WF_vars(Tick)
    /\ WF_vars(Present)

TypeOK ==
    /\ now \in 0..MaxTick
    /\ producer \in 0..MaxProduced
    /\ consumer \in 0..MaxProduced
    /\ consumer <= producer
    /\ loopState \in {Active, Arming, Sleeping}
    /\ observedGeneration \in 0..MaxProduced
    /\ deadline \in 1..MaxTick
    /\ dirty \in BOOLEAN
    /\ frames \in Nat

SleepOwnsExactEmptyGeneration ==
    loopState = Sleeping =>
        /\ consumer = producer
        /\ observedGeneration = producer
        /\ now < deadline

InputNeverSleeps ==
    consumer < producer => loopState = Active

InputEventuallyConsumed ==
    \A sequence \in 1..MaxProduced:
        (producer >= sequence) ~> (consumer >= sequence)

DirtyEventuallyPresented == dirty ~> ~dirty

=============================================================================
